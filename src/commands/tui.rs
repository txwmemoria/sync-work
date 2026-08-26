//! A live dashboard: this machine, its peers, its projects, and what verify
//! has been doing — the answers `sw status`, `sw projects` and the history file
//! hold, on one screen that keeps itself fresh.
//!
//! Read-only by design. Anything that changes state stays a deliberate command;
//! the dashboard is for glancing at while a build runs on the other machine.

use crate::config::{Config, Identity, Machine};
use crate::history::{self, Direction};
use crate::style;
use crate::transport::{self, Event, PROTOCOL_VERSION, Request};
use crate::{git, service};
use anyhow::{Context as _, Result};
use ratatui::crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Everything one frame needs, assembled off-screen by the refresher.
#[derive(Default, Clone)]
struct Snapshot {
    machine_lines: Vec<(String, String)>,
    peer_lines: Vec<(String, PeerHealth, String)>,
    project_lines: Vec<String>,
    history: Vec<history::Entry>,
    refreshed_at: String,
}

/// A peer line's state. `Checking` exists so the first frame can appear
/// immediately — pinging takes a second or more, and blocking the whole screen
/// on it made the dashboard open blank.
#[derive(Clone, PartialEq)]
enum PeerHealth {
    Checking,
    Up,
    Down,
}

pub async fn run() -> Result<i32> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("sw tui needs a terminal");
    }

    let identity = Identity::load_or_create()?;

    // ONE endpoint for the whole session. Binding a fresh one per probe every
    // five seconds — sockets, relay bootstrap, an mDNS discovery actor each
    // time — piled up machinery faster than the async wind-down reclaimed it,
    // and CPU climbed the longer the dashboard ran. The node never had this
    // problem because it binds once; this was the first long-lived repeat
    // binder. Reuse also drops the relay bootstrap from every probe.
    transport::with_endpoint(&identity, |ep| async move {
        let state = Arc::new(Mutex::new(Snapshot::default()));

        // The refresher owns every slow operation — pinging the peer takes a
        // second or more — so the draw loop never blocks and the screen stays
        // responsive while the network does its thing.
        let refresher = {
            let state = state.clone();
            tokio::spawn(async move {
                // A fixed metronome, not sleep-after-work: refresh() itself
                // takes a second or more waiting on peer pings, and adding
                // that on top of the sleep stretched the advertised 5 seconds
                // to 6–8.
                let mut every = tokio::time::interval(Duration::from_secs(5));
                every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    every.tick().await;
                    refresh(&state, &ep).await;
                }
            })
        };

        // Key events come from a blocking reader thread: crossterm's poll/read
        // are synchronous, and parking a runtime worker on them would be wrong.
        let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                if ratatui::crossterm::event::poll(Duration::from_millis(200)).unwrap_or(false)
                    && let Ok(TermEvent::Key(k)) = ratatui::crossterm::event::read()
                    && k.kind == KeyEventKind::Press
                    && key_tx.send(k.code).is_err()
                {
                    return;
                }
            }
        });

        // Colour styling belongs to the widgets now; our own Paint escapes
        // would corrupt the buffer.
        style::set_enabled(false);
        let mut terminal = ratatui::init();
        let outcome = ui_loop(&mut terminal, &state, &mut key_rx).await;
        ratatui::restore();
        refresher.abort();
        outcome
    })
    .await
    .map(|()| 0)
}

async fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &Arc<Mutex<Snapshot>>,
    keys: &mut tokio::sync::mpsc::UnboundedReceiver<KeyCode>,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        let snap = state.lock().map(|s| s.clone()).unwrap_or_default();
        terminal
            .draw(|frame| draw(frame, &snap))
            .context("drawing the dashboard")?;

        tokio::select! {
            _ = tick.tick() => {}
            key = keys.recv() => match key {
                Some(KeyCode::Char('q')) | Some(KeyCode::Esc) | None => return Ok(()),
                _ => {}
            },
        }
    }
}

/// One refresh round, in two stages: local facts are published the moment they
/// are ready, then each peer's ping lands in the shared snapshot as it
/// resolves. Failures degrade to placeholders — a dashboard that dies because
/// one probe failed would be useless exactly when it is needed.
async fn refresh(state: &Arc<Mutex<Snapshot>>, ep: &iroh::Endpoint) {
    let machine = Machine::load().unwrap_or_default();
    let mut snap = local_facts(&machine).await;

    // Carry each peer's previous verdict into the new round: flashing back to
    // "checking…" every 5 seconds would turn the refresh into flicker. Only a
    // peer never probed before shows the placeholder.
    if let Ok(prev) = state.lock() {
        for line in &mut snap.peer_lines {
            if let Some(old) = prev
                .peer_lines
                .iter()
                .find(|(name, health, _)| *name == line.0 && *health != PeerHealth::Checking)
            {
                *line = old.clone();
            }
        }
    }
    if let Ok(mut s) = state.lock() {
        *s = snap;
    }

    let mut probes = tokio::task::JoinSet::new();
    for (i, (name, peer)) in machine.peers.iter().enumerate() {
        let ep = ep.clone();
        let (name, peer) = (name.clone(), peer.clone());
        probes.spawn(async move {
            let verdict = match ping(&ep, &peer).await {
                Ok((build, paths)) => (
                    PeerHealth::Up,
                    format!("{name}  reachable [{build}]  {paths}"),
                ),
                Err(e) => (PeerHealth::Down, format!("{name}  unreachable — {e:#}")),
            };
            (i, verdict)
        });
    }
    while let Some(Ok((i, (health, text)))) = probes.join_next().await {
        if let Ok(mut s) = state.lock()
            && let Some(line) = s.peer_lines.get_mut(i)
        {
            line.1 = health;
            line.2 = text;
        }
    }
}

/// Everything that needs no network: this machine, its projects, the history
/// file. Fast enough that the first frame carries it.
async fn local_facts(machine: &Machine) -> Snapshot {
    let mut snap = Snapshot {
        refreshed_at: style::stamp_now_short(),
        ..Default::default()
    };

    let id = Identity::load_or_create()
        .map(|i| format!("{:.16}…", i.id_string()))
        .unwrap_or_else(|_| "?".into());
    snap.machine_lines = vec![
        ("name".into(), machine.node_name()),
        ("identity".into(), id),
        ("build".into(), env!("SW_BUILD").into()),
        (
            "node".into(),
            match service::running_nodes() {
                Some(0) => "not running".into(),
                Some(1) => "running".into(),
                Some(n) => format!("{n} running (expected one)"),
                None => "unknown".into(),
            },
        ),
        (
            "address".into(),
            transport::self_relay().unwrap_or_else(|| "not established".into()),
        ),
    ];

    for (name, entry) in &machine.projects {
        let path = std::path::Path::new(&entry.path);
        let detail = if !path.is_dir() {
            "path is gone".to_string()
        } else if Config::load(path).is_err() {
            "no syncwork.toml".to_string()
        } else {
            let branch = git::current_branch(path)
                .await
                .unwrap_or_else(|_| "detached".into());
            let dirty = if git::is_dirty(path).await.unwrap_or(false) {
                ", dirty"
            } else {
                ""
            };
            format!("{} ({branch}{dirty})", entry.path)
        };
        snap.project_lines.push(format!("{name:<14} {detail}"));
    }

    for name in machine.peers.keys() {
        snap.peer_lines.push((
            name.clone(),
            PeerHealth::Checking,
            format!("{name}  checking…"),
        ));
    }

    snap.history = history::recent(50);
    snap
}

/// One ping over the session's shared endpoint, returning the peer's build
/// stamp and the connection's paths. The connection is per-probe — dropping
/// it closes it — but the endpoint underneath lives for the whole session.
async fn ping(ep: &iroh::Endpoint, peer: &crate::config::Peer) -> Result<(String, String)> {
    let conn = transport::connect(ep, peer).await?;
    let (mut tx, mut rx) = transport::open_request(&conn).await?;
    transport::write_request(
        &mut tx,
        Request::Ping {
            protocol: PROTOCOL_VERSION,
        },
    )
    .await?;
    tx.finish().ok();
    match transport::read_event(&mut rx).await? {
        Some(Event::Accepted { build, .. }) => Ok((build, transport::describe_paths(&conn).await)),
        _ => anyhow::bail!("no reply"),
    }
}

fn draw(frame: &mut ratatui::Frame, snap: &Snapshot) {
    let rows = Layout::vertical([
        Constraint::Length(2 + snap.machine_lines.len() as u16),
        Constraint::Length(2 + snap.peer_lines.len().max(1) as u16),
        Constraint::Length(2 + snap.project_lines.len().max(1) as u16),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let label = Style::default().fg(Color::DarkGray);
    let block = |title: &'static str| Block::bordered().title(title);

    let machine: Vec<Line> = snap
        .machine_lines
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:<10}"), label),
                Span::raw(v.clone()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(machine).block(block(" machine ")), rows[0]);

    let peers: Vec<Line> = if snap.peer_lines.is_empty() {
        vec![Line::from("none paired")]
    } else {
        snap.peer_lines
            .iter()
            .map(|(_, health, text)| {
                let colour = match health {
                    PeerHealth::Checking => Color::DarkGray,
                    PeerHealth::Up => Color::Green,
                    PeerHealth::Down => Color::Red,
                };
                Line::from(vec![
                    Span::styled("● ", Style::default().fg(colour)),
                    Span::raw(text.clone()),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(peers).block(block(" peers ")), rows[1]);

    let projects: Vec<Line> = if snap.project_lines.is_empty() {
        vec![Line::from("none linked — run `sw link` in a checkout")]
    } else {
        snap.project_lines
            .iter()
            .map(|l| Line::from(l.clone()))
            .collect()
    };
    frame.render_widget(Paragraph::new(projects).block(block(" projects ")), rows[2]);

    let history: Vec<Line> = if snap.history.is_empty() {
        vec![Line::from("no verifies recorded yet")]
    } else {
        snap.history.iter().map(history_line).collect()
    };
    frame.render_widget(
        Paragraph::new(history).block(block(" verify history (newest first) ")),
        rows[3],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" quit    refreshes every 5s"),
            Span::styled(
                format!("    last {}", snap.refreshed_at),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        rows[4],
    );
}

fn history_line(e: &history::Entry) -> Line<'static> {
    let when = style::stamp_at_short(e.ts_ms);
    let arrow = match e.direction {
        Direction::Sent => format!("→ {}", e.peer),
        Direction::Served => format!("← {}", e.peer),
    };
    let (verdict, colour) = match e.code {
        Some(0) => ("ok".to_string(), Color::Green),
        Some(c) => (format!("exit {c}"), Color::Red),
        None => ("failed".to_string(), Color::Red),
    };
    let secs = e.ms as f64 / 1000.0;
    // Columns are padded for alignment, but the two-space gap is unconditional:
    // `{:<22}` alone adds nothing once the content is wider than the column —
    // a long machine name ran straight into the verdict.
    Line::from(vec![
        Span::styled(format!("{when}  "), Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{:<24}  ", format!("{}/{}", e.project, e.recipe))),
        Span::raw(format!("{arrow:<26}  ")),
        Span::styled(format!("{verdict:<8}  "), Style::default().fg(colour)),
        Span::styled(format!("{secs:.1}s"), Style::default().fg(Color::DarkGray)),
    ])
}
