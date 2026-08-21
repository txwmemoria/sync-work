//! Stamps the build with the commit it came from.
//!
//! `sw doctor` needs to answer "is the peer running the code I just pushed?", and
//! the package version cannot: it stays `0.1.0` across every build. That question
//! came up twice during development and both times was resolved by guessing.
//!
//! Falls back to `unknown` rather than failing the build — a checkout without git
//! metadata (a tarball, a vendored copy) should still compile.

use std::process::Command;

fn main() {
    let build = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // A dirty tree is worth surfacing: it means the running binary does not
    // correspond to any commit, so comparing hashes across machines would mislead.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let stamp = if dirty {
        format!("{build}-dirty")
    } else {
        build
    };
    println!("cargo:rustc-env=SW_BUILD={stamp}");

    // HEAD covers commits and branch switches; index covers staging a change,
    // which is what flips the dirty flag most often.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
