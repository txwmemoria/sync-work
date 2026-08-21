//! Keep `sw node` running without anyone remembering to start it.
//!
//! A peer that is not listening makes `sw verify` fail from the other machine,
//! and "did you start the node?" is exactly the papercut this tool exists to
//! remove. Each platform gets its native user-level mechanism — no admin rights
//! anywhere, because this is a development tool and should not need them.
//!
//! One service per *machine*, not per repository: the node it starts serves
//! every project in the machine registry, so linking a new project needs no
//! service changes at all.
//!
//! | platform | mechanism                          |
//! |----------|------------------------------------|
//! | macOS    | launchd LaunchAgent (`~/Library/LaunchAgents`) |
//! | Windows  | Scheduled Task, trigger ONLOGON    |
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

/// Where a service definition lives, and how it is named.
pub struct Plan {
    pub label: String,
    /// File to write, when the platform uses one.
    pub file: Option<PathBuf>,
    pub contents: Option<String>,
    /// Commands to run after writing, in order.
    pub commands: Vec<Vec<String>>,
    pub log: PathBuf,
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
        file: Some(file),
        label,
        log,
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

/// The command a Scheduled Task runs.
///
/// It goes through PowerShell rather than invoking `sw.exe` directly for two
/// reasons: `-WindowStyle Hidden` keeps a console window from appearing at every
/// logon, and the redirect gives the node a log file. Single quotes are the
/// outer quoting so the whole thing survives being embedded in `/TR "..."`.
pub fn schtasks_command(exe: &Path, log: &Path) -> String {
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    format!(
        "powershell -NoProfile -WindowStyle Hidden -Command \"& '{}' node *> '{}'\"",
        q(exe),
        q(log)
    )
}

fn plan_windows(label: String, exe: &Path) -> Result<Plan> {
    let log_dir = directories::BaseDirs::new()
        .map(|b| b.data_local_dir().join("syncwork"))
        .context("cannot determine local data directory")?;
    let log = log_dir.join(format!("{label}.log"));
    Ok(Plan {
        file: None,
        contents: None,
        commands: vec![vec![
            "schtasks".into(),
            "/Create".into(),
            "/TN".into(),
            label.clone(),
            "/TR".into(),
            schtasks_command(exe, &log),
            "/SC".into(),
            "ONLOGON".into(),
            // Least privilege, and it avoids the credential prompt that
            // "run whether logged on or not" would require.
            "/RL".into(),
            "LIMITED".into(),
            // Overwrite an existing task so install is idempotent.
            "/F".into(),
        ]],
        label,
        log,
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
        file: Some(file),
        log: PathBuf::from("journalctl --user -u ".to_string() + &label),
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

/// Render the plan without touching anything.
pub fn print_plan() -> Result<()> {
    let plan = build_plan()?;
    println!("platform  {}", Platform::current());
    println!("label     {}", plan.label);
    println!("log       {}", plan.log.display());
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

    if let Some(parent) = plan.log.parent() {
        // Windows' log path has a real parent; Linux's "log" is a hint string.
        if !parent.as_os_str().is_empty() && Platform::current() != Platform::Linux {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    if let (Some(f), Some(c)) = (&plan.file, &plan.contents) {
        if let Some(parent) = f.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(f, c).with_context(|| format!("writing {}", f.display()))?;
        println!("wrote {}", f.display());
    }

    for (i, cmd) in plan.commands.iter().enumerate() {
        let out = Command::new(&cmd[0]).args(&cmd[1..]).output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                // The first command is a best-effort teardown of any previous
                // install; it legitimately fails when nothing was registered.
                let tolerated = i == 0 && plan.commands.len() > 1;
                if !tolerated {
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

    println!("installed {}", plan.label);
    println!("node logs to {}", plan.log.display());
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
    fn schtasks_command_hides_the_console_and_quotes_paths() {
        let c = schtasks_command(
            Path::new(r"C:\tools\sw.exe"),
            Path::new(r"C:\Users\me\AppData\Local\syncwork\a.log"),
        );
        // A console window at every logon would be unacceptable.
        assert!(c.contains("-WindowStyle Hidden"));
        assert!(c.contains("-NoProfile"));
        assert!(c.contains(r"'C:\tools\sw.exe'"));
        assert!(c.contains("node"));
        // PowerShell's redirect-all operator, so stderr reaches the log too.
        assert!(c.contains("*>"));
        // The outer quoting must stay balanced for /TR "..." to survive.
        assert_eq!(c.matches('"').count(), 2);
        // No `-C <repo>`: the node serves every linked project. Matching the
        // flag with its argument, since `-Command` also starts with `-C`.
        assert!(
            !c.contains("-C '"),
            "service must not be bound to a single repo"
        );
    }

    #[test]
    fn schtasks_command_escapes_embedded_single_quotes() {
        let c = schtasks_command(Path::new(r"C:\it's\sw.exe"), Path::new(r"C:\l.log"));
        // Doubling is how PowerShell escapes a quote inside a single-quoted string.
        assert!(c.contains(r"'C:\it''s\sw.exe'"), "got {c}");
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
    fn the_label_is_machine_scoped() {
        // One node per machine, so the identifier must not vary by project.
        let l = label();
        assert!(l.contains("node"));
        assert_eq!(l, label(), "label must be stable");
    }
}
