# One command to pull, rebuild, restart the node, and watch its log.
#
#   .\scripts\update.ps1
#
# Runs from anywhere: the repo is located from this script's own path, not the
# working directory.
#
# The order is deliberate, and on Windows it is not optional. `cargo install`
# replaces the very binary the node is executing, and Windows refuses to replace
# a running executable — the install fails outright with
# "failed to remove file ...sw.exe". So the node is stopped before the build.

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $repo

function Step($m) { Write-Host "`n==> $m" -ForegroundColor Cyan }
function Warn($m) { Write-Host "    $m" -ForegroundColor Yellow }
function Die($m) { Write-Host "`n==> failed: $m" -ForegroundColor Red; exit 1 }

Step "stopping the node"
# Native commands do not throw on failure regardless of ErrorActionPreference,
# so exit codes are checked explicitly throughout.
if (Get-Command sw -ErrorAction SilentlyContinue) {
    sw service stop
    if ($LASTEXITCODE -ne 0) { Warn "nothing was running" }
} else {
    Warn "sw is not installed yet - skipping"
}

Step "pulling"
# --ff-only: a surprise merge commit during an update is never what you wanted.
git pull --ff-only
if ($LASTEXITCODE -ne 0) { Die "git pull (local commits or a dirty tree?)" }

Step "building and installing"
# --locked so both machines resolve the dependency versions in Cargo.lock rather
# than whatever is newest today.
cargo install --path . --locked
if ($LASTEXITCODE -ne 0) { Die "cargo install" }

Step "starting the node"
sw service start
if ($LASTEXITCODE -ne 0) { Die "sw service start (is it installed? try: sw service install)" }

Step "status"
sw service status

Step "following the log"
Warn "ctrl-c stops watching; the node keeps running"
# Followed with Get-Content rather than `sw service logs -f` on purpose: Windows
# will not replace a running executable, so a lingering `sw` here makes the *next*
# run of this script fail at `cargo install`. This script hit exactly that.
$log = sw service logs --path
if ($LASTEXITCODE -ne 0) { Die "could not locate the log" }
Get-Content -Path $log -Tail 50 -Wait
