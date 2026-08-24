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

    // No `rerun-if-changed` on purpose. Naming `.git/HEAD` and `.git/index` looks
    // more precise but is wrong: editing a tracked file without staging it changes
    // neither, so the stamp kept its old value and omitted `-dirty` — a binary full
    // of uncommitted changes reporting itself as a clean commit, which is exactly
    // the lie `sw doctor`'s build comparison exists to catch. With no directives at
    // all, cargo re-runs this whenever anything in the package changes, which is
    // the condition the stamp actually describes. The cost is two git commands per
    // rebuild.
}
