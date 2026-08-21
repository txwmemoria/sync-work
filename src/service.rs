//! Keep `sw node` running without anyone remembering to start it.
//!
//! A peer that is not listening makes `sw verify` fail from the other machine,
//! and "did you start the node?" is exactly the papercut this tool exists to
//! remove. Each platform gets its native user-level mechanism — no admin rights
//! anywhere, because this is a development tool and should not need them.
//!
//! Windows uses the per-user `Run` key rather than a Scheduled Task: creating a
//! task with a logon trigger is refused outright under the usual corporate
//! policy — `schtasks /Create /SC ONLOGON` returns `Access is denied`, with or
//! without `/RU` and `/IT`. The `Run` key needs no elevation. The cost is that
//! Windows then has no supervisor, so `install` and `restart` start the node
//! themselves and there is no keep-alive.
//!
//! One service per *machine*, not per repository: the node it starts serves
//! every project in the machine registry, so linking a new project needs no
//! service changes at all.
//!
//! | platform | mechanism                          |
//! |----------|------------------------------------|
//! | macOS    | launchd LaunchAgent (`~/Library/LaunchAgents`) |
//! | Windows  | `HKCU\...\CurrentVersion\Run` value  |
//! | Linux    | systemd user unit                  |
//!
//! ## The PATH problem
//!
//! Every one of these starts processes with a minimal environment. A node that
//! launches but cannot find `cargo` is worse than one that does not launch at
//! all: `sw verify` reaches it and then fails with `cargo: not found`, which
//! looks like a broken project rather than a broken install. So the PATH in
//! effect at install time is captured into the service definition.

use crate::config::Platform;
use crate::style::Paint;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Service identifier. There is exactly one node per machine serving every
/// linked project, so this is fixed rather than derived from any project name.
pub fn label() -> String {
    match Platform::current() {
        Platform::Macos => "com.syncwork.node".to_string(),
        _ => "syncwork-node".to_string(),
    }
}

/// Where the node's output ends up.
///
/// Not a path, because on Linux it is not one: systemd captures stdout into the
/// journal, addressed by unit name. Modelling that as a `PathBuf` holding a
/// command string — which this used to do — makes every consumer wrong.
pub enum LogSource {
    File(PathBuf),
    /// systemd journal, addressed by unit name.
    Journal(String),
}

impl LogSource {
    /// How to describe the location to a human.
    pub fn describe(&self) -> String {
        match self {
            Self::File(p) => p.display().to_string(),
            Self::Journal(unit) => format!("systemd journal (unit {unit})"),
        }
    }

    /// The command that shows it.
    fn view_command(&self, follow: bool, lines: u32) -> Vec<String> {
        match self {
            Self::File(p) => {
                if Platform::current() == Platform::Windows {
                    // No tail on Windows; Get-Content -Wait is the equivalent.
                    let wait = if follow { " -Wait" } else { "" };
                    vec![
                        "powershell".into(),
                        "-NoProfile".into(),
                        "-Command".into(),
                        format!(
                            "Get-Content -Path '{}' -Tail {lines}{wait}",
                            p.display().to_string().replace('\'', "''")
                        ),
                    ]
                } else {
                    let mut v = vec!["tail".to_string()];
                    if follow {
                        v.push("-f".into());
                    }
                    v.push("-n".into());
                    v.push(lines.to_string());
                    v.push(p.display().to_string());
                    v
                }
            }
            Self::Journal(unit) => {
                let mut v = vec![
                    "journalctl".to_string(),
                    "--user".into(),
                    "-u".into(),
                    format!("{unit}.service"),
                    "-n".into(),
                    lines.to_string(),
                ];
                if follow {
                    v.push("-f".into());
                }
                v
            }
        }
    }
}

/// Where a service definition lives, and how it is named.
pub struct Plan {
    pub label: String,
    /// File to write, when the platform uses one.
    pub file: Option<PathBuf>,
    pub contents: Option<String>,
    /// Commands to run to completion, in order.
    pub commands: Vec<Vec<String>>,
    /// Commands spawned *without* waiting — the node itself, on a platform with
    /// no supervisor to start it for us.
    pub detached: Vec<Vec<String>>,
    pub log: LogSource,
}

fn home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("cannot determine home directory")
}

fn current_path_env() -> String {
    std::env::var("PATH").unwrap_or_default()
}

// ---------------------------------------------------------------- macOS

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn plist(label: &str, exe: &Path, log: &Path, path_env: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>node</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path_env}</string>
  </dict>
</dict>
</plist>
"#,
        label = xml_escape(label),
        exe = xml_escape(&exe.display().to_string()),
        log = xml_escape(&log.display().to_string()),
        path_env = xml_escape(path_env),
    )
}

fn plan_macos(label: String, exe: &Path) -> Result<Plan> {
    let home = home()?;
    let log = home.join("Library/Logs").join(format!("{label}.log"));
    let file = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    let uid = unsafe_uid();
    Ok(Plan {
        contents: Some(plist(&label, exe, &log, &current_path_env())),
        // `bootstrap` is the supported replacement for `load -w`; booting out
        // first makes the install idempotent.
        commands: vec![
            vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{label}"),
            ],
            vec![
                "launchctl".into(),
                "bootstrap".into(),
                format!("gui/{uid}"),
                file.display().to_string(),
            ],
        ],
        // launchd starts it for us thanks to RunAtLoad.
        detached: Vec::new(),
        file: Some(file),
        label,
        log: LogSource::File(log),
    })
}

/// launchd domains are keyed by numeric uid.
fn unsafe_uid() -> String {
    #[cfg(unix)]
    {
        // Safe in practice: getuid cannot fail and has no preconditions.
        (unsafe { libc_getuid() }).to_string()
    }
    #[cfg(not(unix))]
    {
        "0".to_string()
    }
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

// ---------------------------------------------------------------- Windows

const WIN_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// PowerShell that starts the node with its output redirected to the log.
///
/// Kept in a `.ps1` rather than inlined into the registry value: the expression
/// needs quotes around two paths, and nesting those inside a `reg add /d`
/// argument is a quoting hazard for no benefit. The registry value then only has
/// to quote one plain path.
pub fn windows_launcher_script(exe: &Path, log: &Path) -> String {
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    format!(
        "# Written by `sw service install`. Starts the resident node at logon.\n& '{}' node *> '{}'\n",
        q(exe),
        q(log)
    )
}

/// The `Run` value that executes that script at logon, windowless.
pub fn windows_run_value(script: &Path) -> String {
    format!(
        "powershell -NoProfile -WindowStyle Hidden -File \"{}\"",
        script.display()
    )
}

/// Stop a running node without disturbing short-lived client invocations.
///
/// Matched on the command line rather than the image name: `sw verify` is the
/// same executable, and killing someone's in-flight build would be worse than
/// leaving a stale node running.
fn windows_stop_command() -> Vec<String> {
    // Doubled single quotes are how a single-quoted PowerShell string escapes a
    // quote, which keeps this expression free of double quotes entirely.
    let expr = concat!(
        "Get-CimInstance Win32_Process -Filter 'Name=''sw.exe''' | ",
        "Where-Object { $_.CommandLine -like '* node*' } | ",
        "ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"
    );
    vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        expr.into(),
    ]
}

fn plan_windows(label: String, exe: &Path) -> Result<Plan> {
    let dir = directories::BaseDirs::new()
        .map(|b| b.data_local_dir().join("syncwork"))
        .context("cannot determine local data directory")?;
    let log = dir.join(format!("{label}.log"));
    let script = dir.join("node.ps1");
    Ok(Plan {
        contents: Some(windows_launcher_script(exe, &log)),
        commands: vec![vec![
            "reg".into(),
            "add".into(),
            WIN_RUN_KEY.into(),
            "/v".into(),
            label.clone(),
            "/t".into(),
            "REG_SZ".into(),
            "/d".into(),
            windows_run_value(&script),
            "/f".into(),
        ]],
        // Nothing supervises this on Windows, so `install` starts the node itself
        // rather than making the user log out and back in.
        detached: vec![vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-WindowStyle".into(),
            "Hidden".into(),
            "-File".into(),
            script.display().to_string(),
        ]],
        file: Some(script),
        label,
        log: LogSource::File(log),
    })
}

// ---------------------------------------------------------------- Linux

pub fn systemd_unit(exe: &Path, path_env: &str) -> String {
    format!(
        "[Unit]\n\
         Description=syncwork node\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} node\n\
         Environment=PATH={path_env}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    )
}

fn plan_linux(label: String, exe: &Path) -> Result<Plan> {
    let home = home()?;
    let file = home
        .join(".config/systemd/user")
        .join(format!("{label}.service"));
    Ok(Plan {
        contents: Some(systemd_unit(exe, &current_path_env())),
        commands: vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec![
                "systemctl".into(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                format!("{label}.service"),
            ],
        ],
        // systemd starts it via `enable --now`.
        detached: Vec::new(),
        file: Some(file),
        log: LogSource::Journal(label.clone()),
        label,
    })
}

// ---------------------------------------------------------------- driver

pub fn build_plan() -> Result<Plan> {
    let label = label();
    let exe = std::env::current_exe().context("cannot determine the path to this executable")?;
    match Platform::current() {
        Platform::Macos => plan_macos(label, &exe),
        Platform::Windows => plan_windows(label, &exe),
        Platform::Linux => plan_linux(label, &exe),
    }
}

/// Run a list of commands in order.
///
/// `tolerate_first` covers the teardown step that several platforms need before
/// a (re)start: stopping something that is not running is not an error.
fn run_all(cmds: &[Vec<String>], tolerate_first: bool) -> Result<()> {
    for (i, cmd) in cmds.iter().enumerate() {
        match Command::new(&cmd[0]).args(&cmd[1..]).output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                if !(tolerate_first && i == 0 && cmds.len() > 1) {
                    bail!(
                        "`{}` failed: {}",
                        cmd.join(" "),
                        String::from_utf8_lossy(&o.stderr).trim()
                    );
                }
            }
            Err(e) => bail!("could not run `{}`: {e}", cmd[0]),
        }
    }
    Ok(())
}

/// Spawn commands without waiting, so the node outlives this process.
fn spawn_all(cmds: &[Vec<String>]) -> Result<()> {
    for cmd in cmds {
        Command::new(&cmd[0])
            .args(&cmd[1..])
            .spawn()
            .with_context(|| format!("could not start `{}`", cmd[0]))?;
    }
    Ok(())
}

/// Commands that stop and start the installed service again.
///
/// Needed because `install` is not a restart: on Windows `schtasks /Create /F`
/// rewrites the task definition without touching the process already running
/// from the previous one, so an updated binary would not take effect until the
/// next logon.
fn restart_plan(plan: &Plan) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let label = &plan.label;
    match Platform::current() {
        // `kickstart -k` kills the running instance and starts it again in one
        // step, which is what launchd offers instead of a restart verb.
        Platform::Macos => (
            vec![vec![
                "launchctl".into(),
                "kickstart".into(),
                "-k".into(),
                format!("gui/{}/{label}", unsafe_uid()),
            ]],
            Vec::new(),
        ),
        Platform::Linux => (
            vec![vec![
                "systemctl".into(),
                "--user".into(),
                "restart".into(),
                format!("{label}.service"),
            ]],
            Vec::new(),
        ),
        // No supervisor here: stop it, then start it ourselves.
        Platform::Windows => (vec![windows_stop_command()], plan.detached.clone()),
    }
}

/// Stop and start the service, so an updated binary takes effect.
pub fn restart() -> Result<()> {
    let plan = build_plan()?;
    let (stop, start) = restart_plan(&plan);
    // The stop half is tolerated: restarting something already stopped should
    // just start it.
    run_all(&stop, true).with_context(|| {
        format!(
            "could not restart {}; is it installed? try `sw service status`",
            plan.label
        )
    })?;
    spawn_all(&start)?;
    println!("restarted {}", plan.label);
    println!("node logs to {}", plan.log.describe());
    Ok(())
}

/// Show the node's log, wherever this platform keeps it.
///
/// Exists so nobody has to remember three different answers: a file under
/// `~/Library/Logs`, a file under `%LOCALAPPDATA%`, or a systemd journal unit.
pub fn logs(follow: bool, lines: u32) -> Result<()> {
    let plan = build_plan()?;
    let cmd = plan.log.view_command(follow, lines);
    eprintln!("{} {}", "log".bold(), plan.log.describe().dimmed());
    // stdio is inherited so `--follow` streams instead of buffering.
    let status = Command::new(&cmd[0])
        .args(&cmd[1..])
        .status()
        .with_context(|| format!("could not run `{}`", cmd[0]))?;
    if !status.success() {
        bail!(
            "`{}` exited with {status}. If the node has never run there may be no log yet.",
            cmd.join(" ")
        );
    }
    Ok(())
}

/// Render the plan without touching anything.
pub fn print_plan() -> Result<()> {
    let plan = build_plan()?;
    println!("platform  {}", Platform::current());
    println!("label     {}", plan.label);
    println!("log       {}", plan.log.describe());
    if let (Some(f), Some(c)) = (&plan.file, &plan.contents) {
        println!("\nwould write {}:\n", f.display());
        println!("{c}");
    }
    println!("would run:");
    for cmd in &plan.commands {
        println!("  {}", cmd.join(" "));
    }
    Ok(())
}

pub fn install() -> Result<()> {
    let plan = build_plan()?;

    // The log directory has to exist before the service redirects into it. The
    // journal needs nothing, which is exactly what the enum makes obvious.
    if let LogSource::File(p) = &plan.log
        && let Some(parent) = p.parent()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    if let (Some(f), Some(c)) = (&plan.file, &plan.contents) {
        if let Some(parent) = f.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(f, c).with_context(|| format!("writing {}", f.display()))?;
        println!("wrote {}", f.display());
    }

    // The first command is a best-effort teardown of any previous install; it
    // legitimately fails when nothing was registered yet.
    run_all(&plan.commands, true)?;
    // On platforms without a supervisor this is what actually starts the node.
    spawn_all(&plan.detached)?;

    println!("installed {}", plan.label);
    println!("node logs to {}", plan.log.describe());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let plan = build_plan()?;
    let label = &plan.label;
    let cmd: Vec<String> = match Platform::current() {
        Platform::Macos => {
            vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{}/{label}", unsafe_uid()),
            ]
        }
        Platform::Windows => {
            vec![
                "schtasks".into(),
                "/Delete".into(),
                "/TN".into(),
                label.clone(),
                "/F".into(),
            ]
        }
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "disable".into(),
            "--now".into(),
            format!("{label}.service"),
        ],
    };
    let out = Command::new(&cmd[0]).args(&cmd[1..]).output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!("note: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => bail!("could not run `{}`: {e}", cmd[0]),
    }
    if let Some(f) = &plan.file {
        let _ = std::fs::remove_file(f);
    }
    println!("removed {label}");
    Ok(())
}

pub fn status() -> Result<()> {
    let plan = build_plan()?;
    let label = &plan.label;
    let cmd: Vec<String> = match Platform::current() {
        Platform::Macos => {
            vec![
                "launchctl".into(),
                "print".into(),
                format!("gui/{}/{label}", unsafe_uid()),
            ]
        }
        Platform::Windows => {
            vec![
                "schtasks".into(),
                "/Query".into(),
                "/TN".into(),
                label.clone(),
            ]
        }
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "status".into(),
            format!("{label}.service"),
        ],
    };
    let out = Command::new(&cmd[0])
        .args(&cmd[1..])
        .output()
        .with_context(|| format!("could not run `{}`", cmd[0]))?;
    if out.status.success() {
        println!("{label}: installed");
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().take(12) {
            println!("  {line}");
        }
    } else {
        println!("{label}: not installed");
        println!("  run `sw service install`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_well_formed_and_carries_the_path() {
        let p = plist(
            "com.syncwork.node",
            Path::new("/usr/local/bin/sw"),
            Path::new("/Users/me/Library/Logs/x.log"),
            "/opt/homebrew/bin:/usr/bin",
        );
        assert!(p.starts_with("<?xml"));
        assert!(p.contains("<string>com.syncwork.node</string>"));
        assert!(p.contains("<string>/usr/local/bin/sw</string>"));
        assert!(p.contains("<string>node</string>"));
        assert!(p.contains("<key>RunAtLoad</key>"));
        // Without PATH the node starts but cannot find cargo.
        assert!(p.contains("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(p.matches("<plist").count(), 1);
        assert!(p.trim_end().ends_with("</plist>"));
        // The node is machine-scoped now; a repo argument would pin it to one.
        assert!(
            !p.contains("-C"),
            "service must not be bound to a single repo"
        );
    }

    #[test]
    fn plist_escapes_xml_metacharacters_in_paths() {
        let p = plist(
            "com.syncwork.node",
            Path::new("/Users/me/a&b<c>/sw"),
            Path::new("/tmp/l.log"),
            "/usr/bin",
        );
        assert!(p.contains("/Users/me/a&amp;b&lt;c&gt;/sw"));
        assert!(
            !p.contains("a&b<c>"),
            "raw metacharacters would break the plist"
        );
    }

    #[test]
    fn systemd_unit_restarts_and_carries_the_path() {
        let u = systemd_unit(Path::new("/home/me/.cargo/bin/sw"), "/usr/bin:/bin");
        assert!(u.contains("ExecStart=/home/me/.cargo/bin/sw node"));
        assert!(u.contains("Environment=PATH=/usr/bin:/bin"));
        assert!(u.contains("Restart=always"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(
            !u.contains("-C"),
            "service must not be bound to a single repo"
        );
    }

    #[test]
    fn windows_launcher_hides_the_console_and_redirects_both_streams() {
        let s = windows_launcher_script(
            Path::new(r"C:\tools\sw.exe"),
            Path::new(r"C:\Users\me\AppData\Local\syncwork\a.log"),
        );
        assert!(s.contains(r"& 'C:\tools\sw.exe' node"), "got {s}");
        // `*>` rather than `>`: stderr carries the node's log, so a plain stdout
        // redirect would capture nothing useful.
        assert!(
            s.contains(r"*> 'C:\Users\me\AppData\Local\syncwork\a.log'"),
            "got {s}"
        );
        // No `-C <repo>`: one node serves every linked project.
        assert!(
            !s.contains("-C "),
            "service must not be bound to a single repo"
        );

        let v = windows_run_value(Path::new(r"C:\Users\me\AppData\Local\syncwork\node.ps1"));
        // A console window at every logon would be unacceptable.
        assert!(v.contains("-WindowStyle Hidden"), "got {v}");
        assert!(v.contains("-NoProfile"));
        // Exactly one quoted path. The whole point of the .ps1 indirection is
        // that the registry value never has to nest quotes.
        assert_eq!(v.matches('"').count(), 2, "got {v}");
    }

    #[test]
    fn windows_launcher_escapes_embedded_single_quotes() {
        let s = windows_launcher_script(Path::new(r"C:\it's\sw.exe"), Path::new(r"C:\l.log"));
        // Doubling is how PowerShell escapes a quote inside a single-quoted string.
        assert!(s.contains(r"'C:\it''s\sw.exe'"), "got {s}");
    }

    #[test]
    fn stopping_the_node_does_not_kill_client_invocations() {
        let c = windows_stop_command().join(" ");
        // Filtering on the command line is the whole point: `sw verify` is the
        // same image name, and killing an in-flight build would be worse than
        // leaving a stale node running.
        assert!(c.contains("CommandLine -like '* node*'"), "got {c}");
        assert!(!c.contains("/IM"), "must not match by image name alone");
        // Single-quoted throughout, so it survives being passed as one argument.
        assert!(!c.contains('"'), "got {c}");
    }

    #[test]
    fn restart_stops_before_starting_on_every_platform() {
        // A synthetic plan: only the label and the detached half matter here.
        let plan = Plan {
            label: label(),
            file: None,
            contents: None,
            commands: Vec::new(),
            detached: vec![vec!["launcher".into()]],
            log: LogSource::File(PathBuf::from("/tmp/x.log")),
        };
        let (stop, start) = restart_plan(&plan);
        let stop_flat: Vec<String> = stop.iter().map(|c| c.join(" ")).collect();
        match Platform::current() {
            // launchd has no restart verb; -k is what makes kickstart replace a
            // running instance rather than no-op.
            Platform::Macos => {
                assert!(stop_flat[0].contains("kickstart -k"), "got {stop_flat:?}");
                assert!(start.is_empty(), "launchd restarts it itself");
            }
            Platform::Linux => {
                assert!(stop_flat[0].contains("restart"), "got {stop_flat:?}");
                assert!(start.is_empty(), "systemd restarts it itself");
            }
            // With no supervisor, a restart is only a restart if we start it
            // again ourselves.
            Platform::Windows => {
                assert!(stop_flat[0].contains("Stop-Process"), "got {stop_flat:?}");
                assert_eq!(start, plan.detached, "must relaunch after stopping");
            }
        }
    }

    #[test]
    fn each_platform_views_its_own_kind_of_log() {
        let f = LogSource::File(PathBuf::from("/tmp/n.log"));
        let cmd = f.view_command(true, 20).join(" ");
        if Platform::current() == Platform::Windows {
            // Windows has no tail; -Wait is the streaming equivalent.
            assert!(cmd.contains("Get-Content"), "got {cmd}");
            assert!(cmd.contains("-Wait") && cmd.contains("-Tail 20"));
        } else {
            assert!(cmd.starts_with("tail -f -n 20 "), "got {cmd}");
        }
        // Without --follow the streaming flag must be absent.
        let once = f.view_command(false, 5).join(" ");
        assert!(!once.contains("-Wait"), "got {once}");
        assert!(!once.split(' ').any(|w| w == "-f"), "got {once}");

        // The journal is not a file, which is the whole reason for the enum.
        let j = LogSource::Journal("syncwork-node".into());
        let jc = j.view_command(true, 20).join(" ");
        assert!(
            jc.starts_with("journalctl --user -u syncwork-node.service"),
            "got {jc}"
        );
        assert!(jc.ends_with("-f"));
        assert!(j.describe().contains("journal"));
    }

    #[test]
    fn the_label_is_machine_scoped() {
        // One node per machine, so the identifier must not vary by project.
        let l = label();
        assert!(l.contains("node"));
        assert_eq!(l, label(), "label must be stable");
    }
}
