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
/// The node is handed to `Start-Process` with no stream redirection, and told to
/// write its own log with `--log`.
///
/// Both halves of that are forced, and the reason is one Windows rule: a process
/// created for redirection inherits handles. `CreateProcess` with
/// `bInheritHandles = TRUE` — which is what redirection requires, in Rust and in
/// .NET alike — inherits *every* inheritable handle, not only the streams it was
/// told to replace. So a node started while this process's stdout was a pipe held
/// that pipe open for its entire life, and any caller reading our output
/// (`sw service start | tail`, a shell substitution, a CI step) waited forever for
/// an EOF that would never come. Nulling the child's own streams does not help;
/// the leak is the inheritance, not the assignment. `Start-Process` without any
/// `-Redirect*` argument goes through `ShellExecuteEx`, which does not inherit,
/// and this script then exits, so the pipe closes on time.
///
/// Having the node open its own file also fixes what the log contained. PowerShell's
/// `*>` wrote UTF-16 and wrapped everything the node sent to stderr in
/// `NativeCommandError` records, so `sw service logs` produced spaced-out mojibake
/// with PowerShell diagnostics spliced through the node's startup banner, and
/// `grep` matched nothing.
pub fn windows_launcher_script(exe: &Path, log: &Path) -> String {
    // Doubled single quotes are how a single-quoted PowerShell string escapes a
    // quote, which keeps this free of double quotes entirely.
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    format!(
        "# Written by `sw service install`. Starts the resident node at logon.\n\
         # No redirection here on purpose: a redirected child inherits this\n\
         # process's handles, so the node writes its own log. See\n\
         # windows_launcher_script for the whole story.\n\
         Start-Process -FilePath '{exe}' -ArgumentList 'node','--log','{log}' \
         -WindowStyle Hidden\n",
        exe = q(exe),
        log = q(log),
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
            // Detached from this process's streams, not merely unwaited. An
            // inherited pipe outlives the parent, so a caller that reads our
            // output — a shell substitution, a script, a CI step — waits for EOF
            // on a handle the node holds open for as long as it runs, and
            // `sw service install` appears to hang forever. The node's own output
            // goes to its log file regardless.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("could not start `{}`", cmd[0]))?;
    }
    Ok(())
}

/// Commands that stop the node.
///
/// On launchd this is `bootout` rather than a signal: with `KeepAlive` set, a
/// killed node is immediately restarted, so stopping means leaving the domain.
fn stop_commands(plan: &Plan) -> Vec<Vec<String>> {
    let label = &plan.label;
    match Platform::current() {
        Platform::Macos => vec![vec![
            "launchctl".into(),
            "bootout".into(),
            format!("gui/{}/{label}", unsafe_uid()),
        ]],
        Platform::Linux => vec![vec![
            "systemctl".into(),
            "--user".into(),
            "stop".into(),
            format!("{label}.service"),
        ]],
        Platform::Windows => vec![windows_stop_command()],
    }
}

/// Commands that start it again, as (run-to-completion, spawn-detached).
fn start_commands(plan: &Plan) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let label = &plan.label;
    match Platform::current() {
        Platform::Macos => (
            vec![vec![
                "launchctl".into(),
                "bootstrap".into(),
                format!("gui/{}", unsafe_uid()),
                plan.file
                    .as_ref()
                    .map(|f| f.display().to_string())
                    .unwrap_or_default(),
            ]],
            Vec::new(),
        ),
        Platform::Linux => (
            vec![vec![
                "systemctl".into(),
                "--user".into(),
                "start".into(),
                format!("{label}.service"),
            ]],
            Vec::new(),
        ),
        // No supervisor here, so starting it means spawning it ourselves.
        Platform::Windows => (Vec::new(), plan.detached.clone()),
    }
}

/// Stop the node, tolerating it not running.
///
/// Separate from `restart` because an update has to stop it *before* rebuilding:
/// `cargo install` replaces the very binary the node is executing, and on Windows
/// a running executable cannot be replaced at all.
pub fn stop() -> Result<()> {
    let plan = build_plan()?;
    // Tolerated: stopping something already stopped is success, not failure.
    run_all(&stop_commands(&plan), true)?;
    println!("stopped {}", plan.label);
    Ok(())
}

/// Start the node.
pub fn start() -> Result<()> {
    let plan = build_plan()?;
    let (run, detached) = start_commands(&plan);
    run_all(&run, false).with_context(|| {
        format!(
            "could not start {}; is it installed? try `sw service status`",
            plan.label
        )
    })?;
    spawn_all(&detached)?;
    println!("started {}", plan.label);
    println!("node logs to {}", plan.log.describe());
    Ok(())
}

/// Stop and start the service, so an updated binary takes effect.
pub fn restart() -> Result<()> {
    let plan = build_plan()?;
    run_all(&stop_commands(&plan), true)?;
    let (run, detached) = start_commands(&plan);
    run_all(&run, false).with_context(|| {
        format!(
            "could not restart {}; is it installed? try `sw service status`",
            plan.label
        )
    })?;
    spawn_all(&detached)?;
    println!("restarted {}", plan.label);
    println!("node logs to {}", plan.log.describe());
    Ok(())
}

/// The command that lists resident node processes, one pid per line.
///
/// Both platforms print pids rather than a count so there is a single parsing
/// path. The previous version used `pgrep -c`, which is a GNU extension: BSD
/// pgrep — the one macOS ships — rejects it, produced empty output, and that was
/// read as "no nodes running". Reporting a healthy node as stopped is worse than
/// reporting nothing, so the exit status is now consulted too.
fn node_probe_command() -> Vec<&'static str> {
    match Platform::current() {
        Platform::Windows => vec![
            "powershell",
            "-NoProfile",
            "-Command",
            concat!(
                "Get-CimInstance Win32_Process -Filter 'Name=''sw.exe''' | ",
                "Where-Object { $_.CommandLine -like '* node*' } | ",
                "ForEach-Object { $_.ProcessId }"
            ),
        ],
        // No -c: portable across BSD and GNU pgrep.
        _ => vec!["pgrep", "-f", "sw node"],
    }
}

/// Count the pids a probe printed.
fn count_pids(stdout: &str) -> usize {
    stdout.lines().filter(|l| !l.trim().is_empty()).count()
}

/// How many resident nodes are running here.
///
/// `None` when the probe itself could not be trusted, which is deliberately
/// distinct from zero: "could not tell" is a different instruction to the reader
/// than "not running".
///
/// Matched on the command line rather than the image name for the same reason the
/// stop command is: `sw verify` is the same executable, and counting those would
/// make a healthy machine look like it has stray nodes.
pub fn running_nodes() -> Option<usize> {
    let cmd = node_probe_command();
    let out = Command::new(cmd[0]).args(&cmd[1..]).output().ok()?;
    // pgrep exits 0 when it matched and 1 when it did not; both are answers.
    // Anything else is a usage or system error, and PowerShell only returns 0
    // here, so treat the rest as unknown rather than as zero.
    if !matches!(out.status.code(), Some(0) | Some(1)) {
        return None;
    }
    Some(count_pids(&String::from_utf8_lossy(&out.stdout)))
}

/// Whether the autostart entry exists, without printing anything.
pub fn is_registered() -> bool {
    let Ok(plan) = build_plan() else {
        return false;
    };
    let cmd = status_command(&plan);
    matches!(Command::new(&cmd[0]).args(&cmd[1..]).output(), Ok(o) if o.status.success())
}

/// Show the node's log, wherever this platform keeps it.
///
/// Exists so nobody has to remember three different answers: a file under
/// `~/Library/Logs`, a file under `%LOCALAPPDATA%`, or a systemd journal unit.
pub fn logs(follow: bool, lines: u32, path_only: bool) -> Result<()> {
    let plan = build_plan()?;
    if path_only {
        // Printed so a caller can follow the log with its own tool. That matters
        // on Windows: a long-running `sw` holds an open handle on the installed
        // binary, which makes the next `cargo install` fail — so the update
        // script must not end with `sw` still running.
        match &plan.log {
            LogSource::File(p) => println!("{}", p.display()),
            LogSource::Journal(unit) => println!("journalctl --user -u {unit}.service"),
        }
        return Ok(());
    }
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

/// The command that removes the autostart registration.
fn uninstall_command(plan: &Plan) -> Vec<String> {
    let label = &plan.label;
    match Platform::current() {
        Platform::Macos => {
            vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{}/{label}", unsafe_uid()),
            ]
        }
        Platform::Windows => {
            vec![
                "reg".into(),
                "delete".into(),
                WIN_RUN_KEY.into(),
                "/v".into(),
                label.clone(),
                "/f".into(),
            ]
        }
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "disable".into(),
            "--now".into(),
            format!("{label}.service"),
        ],
    }
}

pub fn uninstall() -> Result<()> {
    let plan = build_plan()?;
    let label = &plan.label;
    let cmd = uninstall_command(&plan);
    let out = Command::new(&cmd[0]).args(&cmd[1..]).output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!("note: {}", String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => bail!("could not run `{}`: {e}", cmd[0]),
    }
    // Removing the autostart entry does not stop what it already started, and on
    // Windows nothing else will.
    let _ = run_all(&stop_commands(&plan), true);
    if let Some(f) = &plan.file {
        let _ = std::fs::remove_file(f);
    }
    println!("removed {label}");
    Ok(())
}

/// The command that reports whether the registration exists.
fn status_command(plan: &Plan) -> Vec<String> {
    let label = &plan.label;
    match Platform::current() {
        Platform::Macos => {
            vec![
                "launchctl".into(),
                "print".into(),
                format!("gui/{}/{label}", unsafe_uid()),
            ]
        }
        Platform::Windows => {
            vec![
                "reg".into(),
                "query".into(),
                WIN_RUN_KEY.into(),
                "/v".into(),
                label.clone(),
            ]
        }
        Platform::Linux => vec![
            "systemctl".into(),
            "--user".into(),
            "status".into(),
            format!("{label}.service"),
        ],
    }
}

pub fn status() -> Result<()> {
    let plan = build_plan()?;
    let label = &plan.label;
    let cmd = status_command(&plan);
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
        assert!(s.contains(r"-FilePath 'C:\tools\sw.exe'"), "got {s}");
        // Start-Process, never a direct call: a foreground node inherits this
        // process's handles and holds any pipe among them open for its whole life,
        // which is what made `sw service start | tail` hang forever.
        assert!(
            !s.contains("& '"),
            "the node must not be run in the foreground: {s}"
        );
        // The node writes its own log. Any `-Redirect*` here would put us back on
        // the inheriting code path, and `*>` would write UTF-16.
        assert!(
            s.contains(r"'node','--log','C:\Users\me\AppData\Local\syncwork\a.log'"),
            "got {s}"
        );
        assert!(
            !s.contains("-Redirect"),
            "redirection re-inherits handles: {s}"
        );
        assert!(!s.contains("*>"), "got {s}");
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
    fn stopping_and_starting_are_symmetric_on_every_platform() {
        // A synthetic plan: only the label, the plist path and the detached half
        // matter here.
        let plan = Plan {
            label: label(),
            file: Some(PathBuf::from("/tmp/agent.plist")),
            contents: None,
            commands: Vec::new(),
            detached: vec![vec!["launcher".into()]],
            log: LogSource::File(PathBuf::from("/tmp/x.log")),
        };
        let stop = stop_commands(&plan)
            .iter()
            .map(|c| c.join(" "))
            .collect::<Vec<_>>();
        let (run, detached) = start_commands(&plan);
        let run_flat = run.iter().map(|c| c.join(" ")).collect::<Vec<_>>();

        match Platform::current() {
            Platform::Macos => {
                // With KeepAlive set, a signal would just be undone by launchd,
                // so stopping has to mean leaving the domain.
                assert!(stop[0].contains("bootout"), "got {stop:?}");
                assert!(run_flat[0].contains("bootstrap"), "got {run_flat:?}");
                assert!(run_flat[0].contains("/tmp/agent.plist"));
                assert!(detached.is_empty());
            }
            Platform::Linux => {
                assert!(stop[0].contains("stop"), "got {stop:?}");
                assert!(run_flat[0].contains("start"), "got {run_flat:?}");
                assert!(detached.is_empty());
            }
            // With no supervisor, starting means spawning it ourselves.
            Platform::Windows => {
                assert!(stop[0].contains("Stop-Process"), "got {stop:?}");
                assert!(run_flat.is_empty(), "nothing to run to completion");
                assert_eq!(detached, plan.detached, "must relaunch the node itself");
            }
        }
    }

    /// install / status / uninstall are three separate matches on the platform,
    /// which is exactly how `status` came to report "not installed" while a node
    /// started by `install` was running: the mechanism was changed in one place
    /// and not the other two.
    #[test]
    fn every_windows_service_command_agrees_on_the_mechanism() {
        if Platform::current() != Platform::Windows {
            return;
        }
        let plan = build_plan().unwrap();
        let mut all: Vec<String> = plan.commands.iter().map(|c| c.join(" ")).collect();
        all.extend(stop_commands(&plan).iter().map(|c| c.join(" ")));
        let (run, _) = start_commands(&plan);
        all.extend(run.iter().map(|c| c.join(" ")));
        all.push(uninstall_command(&plan).join(" "));
        all.push(status_command(&plan).join(" "));

        for cmd in &all {
            assert!(
                !cmd.contains("schtasks"),
                "still reaching for a Scheduled Task, which is refused without                  elevation: {cmd}"
            );
        }
        // The three registry-touching commands must name the same key and value.
        let reg: Vec<&String> = all.iter().filter(|c| c.starts_with("reg ")).collect();
        assert_eq!(reg.len(), 3, "expected add/query/delete, got {reg:?}");
        for cmd in reg {
            assert!(cmd.contains(WIN_RUN_KEY), "wrong key: {cmd}");
            assert!(cmd.contains(&plan.label), "wrong value name: {cmd}");
        }
    }

    #[test]
    fn pids_are_counted_by_line_on_every_platform() {
        assert_eq!(count_pids(""), 0);
        assert_eq!(count_pids("\n"), 0);
        assert_eq!(count_pids("1234\n"), 1);
        assert_eq!(count_pids("1234\n5678\n"), 2);
        // PowerShell emits CRLF; blank trailing lines must not inflate it.
        assert_eq!(count_pids("1234\r\n5678\r\n\r\n"), 2);

        // The probe must not ask for `pgrep -c`: that is a GNU extension, and BSD
        // pgrep rejects it, which is how a running node came to be reported as
        // stopped on macOS.
        let cmd = node_probe_command().join(" ");
        assert!(!cmd.contains(" -c"), "got {cmd}");
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
