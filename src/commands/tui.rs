//! A live dashboard: this machine, its peers, its projects, and what verify
//! has been doing — the answers `sw status`, `sw projects` and the history file
//! hold, on one screen that keeps itself fresh.
//!
//! Read-only by design. Anything that changes state stays a deliberate command;
//! the dashboard is for glancing at while a build runs on the other machine.
//!
//! Each block has its own refresh loop, at its own pace. One combined round
//! meant the slowest thing set the pace for everything: an asleep peer's dial
//! timeout froze the history block too, and `git status` on a large checkout
//! ran as often as the cheap file reads beside it.

use crate::config::{CONFIG_FILE, Config, Identity, Machine};
use crate::history::{self, Direction};
use crate::style;
use crate::transport::{self, Event, PROTOCOL_VERSION, Request};
use crate::{git, node};
use anyhow::{Context as _, Result};
use ratatui::crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Machine facts and history: file reads only, so they can be frequent.
const LOCAL_EVERY: Duration = Duration::from_secs(5);
/// One ping per peer per round.
const PEERS_EVERY: Duration = Duration::from_secs(5);
/// `git status` walks the whole working tree; a large C++ checkout is not
/// something to scan every five seconds.
const PROJECTS_EVERY: Duration = Duration::from_secs(15);
/// Past this a peer is reported down rather than left "checking" until iroh's
/// own dial timeout gives up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything one frame needs. Each refresh loop owns some of the fields and
/// writes only those.
#[derive(Default, Clone)]
struct Snapshot {
    machine_lines: Vec<(String, String)>,
    peer_lines: Vec<(String, PeerHealth, String)>,
    /// `None` until the first project scan finishes, so the screen can say
    /// "checking" rather than the untrue "none linked".
    project_lines: Option<Vec<String>>,
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

type Shared = Arc<Mutex<Snapshot>>;

pub async fn run() -> Result<i32> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("sw tui needs a terminal");
    }

    let identity = Identity::load_or_create()?;
    let id = format!("{:.16}…", identity.id_string());

    // ONE endpoint for the whole session. Binding a fresh one per probe every
    // five seconds — sockets, relay bootstrap, an mDNS discovery actor each
    // time — is machinery no dashboard needs to rebuild, and reuse also drops
    // the relay bootstrap from every probe after the first.
    transport::with_endpoint(&identity, |ep| async move {
        let state: Shared = Arc::default();
        // Local facts are cheap: have them before the first frame is drawn.
        refresh_local(&state, &id);

        let loops = [
            tokio::spawn(every(LOCAL_EVERY, {
                let state = state.clone();
                move || {
                    refresh_local(&state, &id);
                    std::future::ready(())
                }
            })),
            tokio::spawn(every(PEERS_EVERY, {
                let state = state.clone();
                move || refresh_peers(state.clone(), ep.clone())
            })),
            tokio::spawn(every(PROJECTS_EVERY, {
                let state = state.clone();
                move || refresh_projects(state.clone())
            })),
        ];

        // Key events come from a blocking reader thread: crossterm's poll/read
        // are synchronous, and parking a runtime worker on them would be wrong.
        let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                match ratatui::crossterm::event::poll(Duration::from_millis(200)) {
                    Ok(false) => {}
                    Ok(true) => {
                        if let Ok(TermEvent::Key(k)) = ratatui::crossterm::event::read()
                            && k.kind == KeyEventKind::Press
                            && key_tx.send(k.code).is_err()
                        {
                            return;
                        }
                    }
                    // A failing poll fails instantly, every time — retrying it
                    // is a busy loop pinning a core. Without a readable
                    // terminal there is no way to press q either, so end the
                    // thread; dropping the sender ends the dashboard.
                    Err(_) => return,
                }
            }
        });

        // Colour styling belongs to the widgets now; our own Paint escapes
        // would corrupt the buffer.
        style::set_enabled(false);
        let mut terminal = ratatui::init();
        let outcome = ui_loop(&mut terminal, &state, &mut key_rx).await;
        ratatui::restore();
        for task in loops {
            task.abort();
        }
        outcome
    })
    .await
    .map(|()| 0)
}

/// Run `round` on a fixed metronome. Measured from the start of each round,
/// not sleep-after-work, so a slow round does not stretch the advertised
/// period; `Delay` keeps one overrun from causing a burst of catch-up rounds.
async fn every<F, Fut>(period: Duration, mut round: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        round().await;
    }
}

async fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &Shared,
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

/// This machine and the history file — no network, no subprocess.
fn refresh_local(state: &Shared, id: &str) {
    let machine = Machine::load().unwrap_or_default();
    let machine_lines = vec![
        ("name".into(), machine.node_name()),
        ("identity".into(), id.to_string()),
        ("build".into(), env!("SW_BUILD").into()),
        (
            "node".into(),
            match node::is_running() {
                Some(true) => "running".into(),
                Some(false) => "not running".into(),
                None => "unknown".into(),
            },
        ),
        (
            "address".into(),
            transport::self_relay().unwrap_or_else(|| "not established".into()),
        ),
    ];
    let history = history::recent(50);
    if let Ok(mut s) = state.lock() {
        s.machine_lines = machine_lines;
        s.history = history;
        s.refreshed_at = style::stamp_at_short(crate::ctx::now_ms());
    }
}

/// Ping every peer concurrently; each verdict lands on screen as it arrives.
async fn refresh_peers(state: Shared, ep: iroh::Endpoint) {
    let machine = Machine::load().unwrap_or_default();

    // Re-list the peers (pairing may have changed), carrying each one's
    // previous verdict: flashing back to "checking…" every round would turn
    // the refresh into flicker. Only a peer never probed shows the placeholder.
    if let Ok(mut s) = state.lock() {
        let lines = machine
            .peers
            .keys()
            .map(|name| {
                s.peer_lines
                    .iter()
                    .find(|(n, health, _)| n == name && *health != PeerHealth::Checking)
                    .cloned()
                    .unwrap_or_else(|| {
                        (
                            name.clone(),
                            PeerHealth::Checking,
                            format!("{name}  checking…"),
                        )
                    })
            })
            .collect();
        s.peer_lines = lines;
    }

    let mut probes = tokio::task::JoinSet::new();
    for (name, peer) in machine.peers {
        let ep = ep.clone();
        probes.spawn(async move {
            let verdict = match tokio::time::timeout(PROBE_TIMEOUT, ping(&ep, &peer)).await {
                Ok(Ok((build, paths))) => (
                    PeerHealth::Up,
                    format!("{name}  reachable [{build}]  {paths}"),
                ),
                Ok(Err(e)) => (PeerHealth::Down, format!("{name}  unreachable — {e:#}")),
                Err(_) => (
                    PeerHealth::Down,
                    format!(
                        "{name}  no answer within {}s — is `sw node` running there?",
                        PROBE_TIMEOUT.as_secs()
                    ),
                ),
            };
            (name, verdict)
        });
    }
    // A probe that panicked is skipped, not a reason to abandon the others.
    while let Some(joined) = probes.join_next().await {
        let Ok((name, (health, text))) = joined else {
            continue;
        };
        if let Ok(mut s) = state.lock()
            && let Some(line) = s.peer_lines.iter_mut().find(|(n, _, _)| *n == name)
        {
            line.1 = health;
            line.2 = text;
        }
    }
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

/// Branch and dirtiness of every linked checkout, scanned concurrently.
async fn refresh_projects(state: Shared) {
    let machine = Machine::load().unwrap_or_default();
    let mut scans = tokio::task::JoinSet::new();
    for (name, entry) in machine.projects {
        scans.spawn(async move {
            let path = std::path::PathBuf::from(&entry.path);
            let detail = if !path.is_dir() {
                "path is gone".to_string()
            } else if !path.join(CONFIG_FILE).is_file() {
                format!("no {CONFIG_FILE}")
            } else if Config::load(&path).is_err() {
                // Present but unreadable is a different fix from absent;
                // doctor prints the parse error in full.
                format!("{CONFIG_FILE} has errors — run `sw doctor`")
            } else {
                let (branch, dirty) =
                    tokio::join!(git::current_branch(&path), git::is_dirty(&path));
                let branch = branch.unwrap_or_else(|_| "detached".into());
                let dirty = if dirty.unwrap_or(false) {
                    ", dirty"
                } else {
                    ""
                };
                format!("{} ({branch}{dirty})", entry.path)
            };
            format!("{name:<14} {detail}")
        });
    }
    let mut lines: Vec<String> = scans.join_all().await;
    lines.sort();
    if let Ok(mut s) = state.lock() {
        s.project_lines = Some(lines);
    }
}

fn draw(frame: &mut ratatui::Frame, snap: &Snapshot) {
    let project_rows = snap.project_lines.as_ref().map_or(1, |l| l.len().max(1));
    let rows = Layout::vertical([
        Constraint::Length(2 + snap.machine_lines.len() as u16),
        Constraint::Length(2 + snap.peer_lines.len().max(1) as u16),
        Constraint::Length(2 + project_rows as u16),
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

    let projects: Vec<Line> = match &snap.project_lines {
        None => vec![Line::styled("checking…", label)],
        Some(lines) if lines.is_empty() => {
            vec![Line::from("none linked — run `sw link` in a checkout")]
        }
        Some(lines) => lines.iter().map(|l| Line::from(l.clone())).collect(),
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
            Span::raw(format!(
                " quit    peers and history every {}s, projects every {}s",
                PEERS_EVERY.as_secs(),
                PROJECTS_EVERY.as_secs()
            )),
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
