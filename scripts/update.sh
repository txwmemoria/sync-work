#!/usr/bin/env bash
# One command to pull, rebuild, restart the node, and watch its log.
#
#   scripts/update.sh
#
# Runs from anywhere: the repo is located from this script's own path, not the
# working directory.
#
# The order is deliberate. `cargo install` replaces the very binary the node is
# executing, so the node is stopped *before* the build, not after. On macOS a
# running executable can be replaced silently, which is worse than failing: the
# install appears to succeed while the old node keeps serving requests.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
warn() { printf '\033[1;33m    %s\033[0m\n' "$1"; }
die() { printf '\n\033[1;31m==> failed: %s\033[0m\n' "$1"; exit 1; }

step "stopping the node"
if command -v sw > /dev/null 2>&1; then
  sw service stop || warn "nothing was running"
else
  warn "sw is not installed yet — skipping"
fi

step "pulling"
# --ff-only: a surprise merge commit during an update is never what you wanted.
git pull --ff-only || die "git pull (local commits or a dirty tree?)"

step "building and installing"
# --locked so both machines resolve the dependency versions in Cargo.lock rather
# than whatever is newest today.
cargo install --path . --locked || die "cargo install"

step "starting the node"
sw service start || die "sw service start (is it installed? try: sw service install)"

step "status"
sw service status

step "following the log"
warn "ctrl-c stops watching; the node keeps running"
sw service logs -f
