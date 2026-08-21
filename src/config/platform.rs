//! Which operating system a node is running.
//!
//! Kept separate from the rest of the configuration because it is not
//! configuration at all — it is a fact about the host, consulted by recipes,
//! service installation, and the wire protocol alike.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }

    /// Shell used to run recipe steps. Recipe steps are written once and must
    /// behave the same on both machines, so we deliberately do not use cmd.exe.
    pub fn shell(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Windows => ("powershell", &["-NoProfile", "-NonInteractive", "-Command"]),
            _ => ("/bin/sh", &["-c"]),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
        };
        f.write_str(s)
    }
}
