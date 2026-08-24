//! Terminal styling, switchable at runtime.
//!
//! This exists rather than a colour crate because the requirement is a *global*
//! on/off decided once at startup, applied uniformly. `owo-colors` offers that
//! only through its `if_supports_color` wrapper — its plain `.bold()` / `.green()`
//! methods emit escapes unconditionally, so a `set_override(false)` has no effect
//! on them. Getting it wrong is silent: output looks fine in a terminal and
//! quietly corrupts every redirected log.
//!
//! Escape codes belong in a terminal and nowhere else. `sw verify > build.log`
//! must produce plain text, and the end-to-end tests depend on being able to
//! grep output without stripping escapes first.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called once from `main`, before anything is printed.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Whether to decorate output at all.
///
/// `NO_COLOR` is honoured because it is the established convention, and stderr
/// is the stream tested because that is where progress and diagnostics go.
pub fn detect() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stderr().is_terminal()
}

// ---------------------------------------------------------------- timestamps

/// The one timestamp format, shared by the tracing log and stamped output lines.
///
/// A single definition rather than two copies that happen to match: "the watch
/// output looks like the log" is a promise, and two format strings drift the
/// first time one of them is touched.
pub const TIME_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3][offset_hour \
         sign:mandatory]:[offset_minute]"
);

/// The machine's UTC offset, captured in `main` while the process is still
/// single-threaded — reading it later is refused by the `time` crate (another
/// thread could be in `setenv` while `localtime_r` reads the environment).
static LOCAL_OFFSET: std::sync::OnceLock<time::UtcOffset> = std::sync::OnceLock::new();

pub fn set_local_offset(offset: time::UtcOffset) {
    let _ = LOCAL_OFFSET.set(offset);
}

/// Whether output lines carry a leading timestamp.
///
/// Off by default and switched on by long-running modes (`sw ctx --watch`): their
/// lines arrive hours apart, so *when* is part of the information. A one-shot
/// command's output all happens "now", and stamping it would only add noise.
static STAMPED: AtomicBool = AtomicBool::new(false);

pub fn set_stamped(on: bool) {
    STAMPED.store(on, Ordering::Relaxed);
}

/// The line prefix: a timestamp in the log's exact format, or nothing.
///
/// Falls back to UTC when the offset could not be captured, which is also what
/// the log does — the two must stay in step even in their failure mode. Dimmed
/// like the log's own timestamps, and for the same reason: the stamp is context,
/// not content.
pub fn stamp() -> String {
    if !STAMPED.load(Ordering::Relaxed) {
        return String::new();
    }
    let now = match LOCAL_OFFSET.get() {
        Some(off) => time::OffsetDateTime::now_utc().to_offset(*off),
        None => time::OffsetDateTime::now_utc(),
    };
    match now.format(&TIME_FORMAT) {
        Ok(s) => format!("{} ", s.dimmed()),
        Err(_) => String::new(),
    }
}

/// Styling helpers returning plain `String`s.
///
/// Chaining works — `"ok".green().bold()` nests two sequences, which renders as
/// bold green and collapses to the bare text when styling is off.
pub trait Paint: Display {
    fn sgr(&self, code: &str) -> String {
        if enabled() {
            format!("\x1b[{code}m{self}\x1b[0m")
        } else {
            self.to_string()
        }
    }

    fn bold(&self) -> String {
        self.sgr("1")
    }
    fn dimmed(&self) -> String {
        self.sgr("2")
    }
    fn red(&self) -> String {
        self.sgr("31")
    }
    fn green(&self) -> String {
        self.sgr("32")
    }
    fn yellow(&self) -> String {
        self.sgr("33")
    }
    fn blue(&self) -> String {
        self.sgr("34")
    }
    fn cyan(&self) -> String {
        self.sgr("36")
    }
}

impl<T: Display + ?Sized> Paint for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log filter is built from the crate name, and a tracing target defaults
    /// to `module_path!()` — whose root is the *binary* target name, not the
    /// package name. Spelling the package name into the filter silently dropped
    /// every event the node emitted, which is invisible until someone goes
    /// looking for a log that is not there.
    #[test]
    fn the_crate_name_used_by_the_log_filter_matches_the_module_path() {
        assert_eq!(env!("CARGO_CRATE_NAME"), "sw");
        assert!(
            module_path!().starts_with(env!("CARGO_CRATE_NAME")),
            "module path {} does not start with {}",
            module_path!(),
            env!("CARGO_CRATE_NAME")
        );
    }

    /// The stamp is process-global like styling, so all its states live in one
    /// test. The format assertion is the important one: it pins the stamp to the
    /// log's shape, which is the entire promise of `TIME_FORMAT` being shared.
    #[test]
    fn stamps_match_the_log_format_and_vanish_when_off() {
        set_enabled(false);

        // Off (the default): one-shot commands must not grow prefixes.
        set_stamped(false);
        assert_eq!(stamp(), "");

        set_stamped(true);
        let s = stamp();
        // e.g. `2026-08-24 14:12:01.856+08:00 ` — date, ms precision, mandatory
        // sign on the offset, one trailing space separating it from the line.
        // (Colour is off above, so no escape codes are in the way.)
        assert_eq!(s.len(), "2026-08-24 14:12:01.856+08:00 ".len(), "got {s:?}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert_eq!(&s[19..20], ".");
        assert!(
            s[23..24] == *"+" || s[23..24] == *"-",
            "offset sign is mandatory, got {s:?}"
        );
        assert_eq!(&s[26..27], ":");
        assert!(
            s.ends_with(' '),
            "the stamp must separate itself from the line"
        );

        set_stamped(false);
    }

    /// Styling is process-global, so these assertions have to run in one test to
    /// avoid racing each other under the parallel test harness.
    #[test]
    fn styling_is_suppressed_when_disabled_and_applied_when_enabled() {
        set_enabled(false);
        assert_eq!("ok".green(), "ok");
        assert_eq!("ok".green().bold(), "ok", "chained styles must also vanish");
        assert_eq!(42.dimmed(), "42");
        assert!(!"x".red().contains('\x1b'), "no escapes may reach a pipe");

        set_enabled(true);
        assert_eq!("ok".green(), "\x1b[32mok\x1b[0m");
        // Chaining nests: the text is preceded by both codes, so it renders with
        // both attributes.
        assert_eq!("ok".green().bold(), "\x1b[1m\x1b[32mok\x1b[0m\x1b[0m");
        assert!("x".bold().contains('\x1b'));

        // Leave the global off so unrelated tests see plain output.
        set_enabled(false);
    }
}
