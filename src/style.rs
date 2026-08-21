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

    /// Styling is process-global, so these assertions have to run in one test to
    /// avoid racing each other under the parallel test harness.
    #[test]
    fn styling_is_suppressed_when_disabled_and_applied_when_enabled() {
        set_enabled(false);
        assert_eq!("ok".green(), "ok");
        assert_eq!("ok".green().bold(), "ok", "chained styles must also vanish");
        assert_eq!(format!("{}", 42.dimmed()), "42");
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
