#!/usr/bin/env bash
# End-to-end test: two machines, TWO projects, one node, one pairing.
#
# This exercises what unit tests cannot: machine-level pairing, the project
# registry, the QUIC transport, patch transfer, worktree isolation, diagnostic
# round-tripping, exit-code propagation, and context sync. Both "machines" run on
# this host, distinguished by SW_HOME.
#
#   bash tests/e2e.sh
#
# Requires: a debug build (`cargo build`), git, and a working network path to
# iroh's discovery infrastructure.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SW="$REPO_ROOT/target/debug/sw"
[ -x "$SW" ] || SW="$SW.exe"
[ -x "$SW" ] || { echo "no debug binary; run: cargo build"; exit 1; }

LAB="${TMPDIR:-/tmp}/sw-e2e-$$"
rm -rf "$LAB"; mkdir -p "$LAB"/{hA,hB,remotes}
cd "$LAB"

step() { printf '\n\033[1;36m== %s\033[0m\n' "$1"; }
fail() { printf '\033[1;31mFAIL: %s\033[0m\n' "$1"; exit 1; }
cleanup() {
  if [ -n "${NODE_PID:-}" ]; then
    kill "$NODE_PID" 2>/dev/null
    # The node holds file handles inside $LAB; removing it while the process is
    # still exiting fails with "device or resource busy" on Windows.
    wait "$NODE_PID" 2>/dev/null
  fi
  cd / || return
  rm -rf "$LAB" 2>/dev/null
}
# INT/TERM as well as EXIT: a Ctrl-C part way through would otherwise leave the
# node running and holding file handles under $LAB.
trap cleanup EXIT INT TERM

# Machine A is the one that initiates; machine B runs the node.
a() { (cd "$1" && SW_HOME="$LAB/hA" CLAUDE_CONFIG_DIR="$LAB/claudeA" "$SW" "${@:2}"); }
b() { (cd "$1" && SW_HOME="$LAB/hB" CLAUDE_CONFIG_DIR="$LAB/claudeB" "$SW" "${@:2}"); }

# ---------------------------------------------------------------- fixtures
make_project() {  # $1 = project name, $2 = lib.rs body
  local name="$1" body="$2"
  git init --quiet --bare "remotes/$name.git"
  mkdir -p "A/$name/src"
  (
    cd "A/$name"
    git init --quiet && git config user.email t@t && git config user.name t
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$name" > Cargo.toml
    printf '%s\n' "$body" > src/lib.rs
    echo target > .gitignore
    git add -A && git commit --quiet -m init
    git remote add origin "../../remotes/$name.git"
    git push --quiet -u origin HEAD 2>/dev/null
  )
}

step "fixtures: two independent cargo projects, each with a bare remote"
mkdir -p A B
make_project widget 'pub fn add(a: i32, b: i32) -> i32 { a + b }'
make_project gadget 'pub fn mul(a: i32, b: i32) -> i32 { a * b }'
echo ok

step "sw init in each project on A, then clone to B"
for name in widget gadget; do
  a "A/$name" init --name "$name" > /dev/null || fail "init $name"
  # Context tests create files and sync them immediately; the quiet window that
  # protects live sessions would otherwise skip every one of them.
  (cd "A/$name" && sed -i 's/quiet_secs = 5/quiet_secs = 0/' syncwork.toml \
     && grep -q "quiet_secs = 0" syncwork.toml || fail "could not disable quiet window")
  (cd "A/$name" && git add -A && git commit --quiet -m "add syncwork.toml" \
     && git push --quiet origin HEAD)
  git clone --quiet "remotes/$name.git" "B/$name"
  (cd "B/$name" && git config user.email t@t && git config user.name t)
  # The committed config must not carry machine state — that was the old design.
  grep -q '^\[peers' "A/$name/syncwork.toml" && fail "syncwork.toml still holds a peer table"
done
echo ok

step "an unpaired machine reports cleanly, with no library noise"
# A third home that never gets paired — the state a first-time user is in.
mkdir -p "$LAB/hC"
(cd "$LAB" && SW_HOME="$LAB/hC" "$SW" status) > "$LAB/unpaired.out" 2> "$LAB/unpaired.err"
RC=$?
[ $RC -ne 0 ] || fail "status on an unpaired machine should exit non-zero"
grep -q "sw pair init" "$LAB/unpaired.err" || { cat "$LAB/unpaired.err"; fail "error does not say how to fix it"; }
# The endpoint must not be bound at all when there is nobody to dial; binding and
# then returning early makes iroh stamp its own error over the real message.
grep -q "Endpoint dropped" "$LAB/unpaired.err" \
  && { cat "$LAB/unpaired.err"; fail "iroh shutdown noise leaked into the error"; }
echo "PASS clean error, exit $RC"

# ---------------------------------------------------------------- pairing
step "pair the MACHINES once, from outside any repository"
# $LAB is deliberately not a git repo: machine-scoped commands must not need one.
TICKET_A=$(a "$LAB" pair init --name node-a | grep -o 'sw1:[A-Z0-9]*')
[ -n "$TICKET_A" ] || fail "pair init failed outside a repo"
TICKET_B=$(b "$LAB" pair join "$TICKET_A" --name node-b | grep -o 'sw1:[A-Z0-9]*')
[ -n "$TICKET_B" ] || fail "pair join failed outside a repo"
a "$LAB" pair join "$TICKET_B" --name node-a > /dev/null || fail "A join"
# Exactly one peer each, and no self-entry.
[ "$(a "$LAB" pair list | grep -c '^node-b')" -eq 1 ] || fail "A does not see exactly one peer"
[ "$(b "$LAB" pair list | grep -c '^node-a')" -eq 1 ] || fail "B does not see exactly one peer"
echo ok

step "link both projects on both machines"
for name in widget gadget; do
  a "A/$name" link > /dev/null || fail "link $name on A"
  b "B/$name" link > /dev/null || fail "link $name on B"
done
[ "$(a "$LAB" projects | grep -cE '^(widget|gadget)')" -eq 2 ] || fail "A should list 2 projects"
[ "$(b "$LAB" projects | grep -cE '^(widget|gadget)')" -eq 2 ] || fail "B should list 2 projects"
echo ok

# ---------------------------------------------------------------- node
step "start ONE node on B, serving every linked project"
# `exec` matters: without it $! is the subshell's pid, and killing that leaves the
# node orphaned — holding file handles under $LAB and, worse, still answering
# requests during later runs. With exec the subshell *becomes* the node.
(cd "$LAB" && exec env SW_HOME="$LAB/hB" CLAUDE_CONFIG_DIR="$LAB/claudeB" "$SW" node \
   > "$LAB/node.log" 2>&1) &
NODE_PID=$!
for _ in $(seq 1 60); do
  grep -q "waiting for requests" "$LAB/node.log" 2>/dev/null && break
  sleep 0.5
done
grep -q "waiting for requests" "$LAB/node.log" || { cat "$LAB/node.log"; fail "node did not start"; }
grep -q "widget" "$LAB/node.log" || fail "node does not know about widget"
grep -q "gadget" "$LAB/node.log" || fail "node does not know about gadget"
echo ok

step "sw status works from outside a repo and reports both projects"
OUT=$(a "$LAB" status) || { echo "$OUT"; fail "status"; }
grep -q "widget" <<<"$OUT" || { echo "$OUT"; fail "status did not report widget"; }
grep -q "gadget" <<<"$OUT" || fail "status did not report gadget"
echo ok

# ------------------------------------------------- one node, two projects
step "verify BOTH projects over the single node and single pairing"
for name in widget gadget; do
  a "A/$name" verify ci > "$LAB/$name.out" 2> "$LAB/$name.err"
  [ $? -eq 0 ] || { tail -20 "$LAB/$name.err"; fail "$name verify did not return 0"; }
  grep -q "B/$name" "$LAB/$name.err" || { cat "$LAB/$name.err"; fail "$name built in the wrong checkout"; }
done
echo "PASS both projects verified"

# ------------------------------------- uncommitted error fails with diagnostic
step "the node records each request it served"
# The node streams stages and build output to the *requester*, so its own log is
# the only place the operator can see that a request arrived at all. Nothing else
# in this suite covers it: the banner is eprintln, the request lines are tracing,
# and only the banner was ever asserted on.
grep -q "verify widget/ci" "$LAB/node.log"   || { tail -20 "$LAB/node.log"; fail "node log has no record of the request"; }
grep -q -- "-> exit 0" "$LAB/node.log"   || { tail -20 "$LAB/node.log"; fail "node log has no record of the outcome"; }
# The path must be reported after the work, not at connect time: a connection is
# always relayed at the moment it opens, so sampling it there would claim every
# transfer went through the relay.
grep -q "served via" "$LAB/node.log"   || { tail -20 "$LAB/node.log"; fail "node log does not report the path it served over"; }
echo "PASS node log shows request and outcome"

step "UNCOMMITTED compile error should fail with a located diagnostic"
cat > "A/widget/src/lib.rs" <<'EOF'
pub fn add(a: i32, b: i32) -> i32 { a + b }
pub fn broken() -> String { let x: i32 = "not an int"; x.to_string() }
EOF
a "A/widget" verify ci > "$LAB/v2.out" 2> "$LAB/v2.err"
RC=$?
[ $RC -ne 0 ] || fail "expected non-zero exit for a broken build"
grep -q "E0308\|mismatched types" "$LAB/v2.err" || fail "no compiler diagnostic came back"
grep -q "lib.rs" "$LAB/v2.err" || fail "diagnostic lost its file location"
grep -q "reusing worktree" "$LAB/v2.err" || fail "worktree was not reused"
echo "PASS exit $RC, diagnostic located"

step "neither side was polluted"
(cd "A/widget" && git diff --cached --quiet) || fail "verify dirtied A's staging area"
[ -z "$(cd "A/widget" && git stash list)" ] || fail "verify left a stash behind"
grep -q "not an int" "B/widget/src/lib.rs" && fail "patch leaked into B's main checkout"
[ -d "B/widget/.sw/wt/node-a" ] || fail "no worktree on B"
grep -q "not an int" "B/widget/.sw/wt/node-a/src/lib.rs" || fail "patch never reached B's worktree"
# The other project must be entirely untouched by widget's traffic.
grep -q "not an int" "B/gadget/src/lib.rs" && fail "widget's patch leaked into gadget"
echo "PASS isolation holds, across projects too"

step "fix and re-verify"
printf 'pub fn add(a: i32, b: i32) -> i32 { a + b }\n' > "A/widget/src/lib.rs"
a "A/widget" verify ci > "$LAB/v4.out" 2> "$LAB/v4.err"
[ $? -eq 0 ] || { tail -20 "$LAB/v4.err"; fail "fixed verify failed"; }
echo "PASS exit 0"

step "an unlinked project on the peer gives an actionable error"
b "$LAB" unlink gadget > /dev/null || fail "unlink"
a "A/gadget" verify ci > "$LAB/v5.out" 2> "$LAB/v5.err"
RC=$?
[ $RC -ne 0 ] || fail "verify should fail when the peer has not linked the project"
grep -q "not linked on node-b" "$LAB/v5.err" \
  || { cat "$LAB/v5.err"; fail "error does not name the machine"; }
grep -q "sw link" "$LAB/v5.err" || fail "error does not say how to fix it"
b "B/gadget" link > /dev/null || fail "re-link"
echo "PASS actionable error, recovered by linking"

step "stderr carries no library noise"
for f in "$LAB"/*.err; do
  grep -q "Endpoint dropped without calling" "$f" && fail "ungraceful shutdown leaked into $f"
done
echo "PASS clean stderr"

# ================================================================ sw ctx
claude_dir() {  # $1 = machine letter, $2 = project
  local top; top="$(cd "$LAB/$1/$2" && git rev-parse --show-toplevel)"
  # Mirrors ctx::encode_project_dir: anything outside [A-Za-z0-9-] becomes '-'.
  printf '%s/projects/%s' "$LAB/claude$1" "$(printf '%s' "$top" | sed 's/[^A-Za-z0-9-]/-/g')"
}
CLAUDE_A="$(claude_dir A widget)"
CLAUDE_B="$(claude_dir B widget)"
# The two sides must genuinely disagree on the directory name, or these tests
# would pass even if path derivation were broken.
[ "$(basename "$CLAUDE_A")" != "$(basename "$CLAUDE_B")" ] \
  || fail "fixture is degenerate: both sides encode to the same directory name"

step "ctx: session state moves both directions despite different dir names"
mkdir -p "$CLAUDE_A/memory" "$CLAUDE_B"
printf '{"turn":1}\n' > "$CLAUDE_A/sess.jsonl"
printf -- '- [Note](n.md)\n' > "$CLAUDE_A/memory/MEMORY.md"
printf 'from B\n' > "$CLAUDE_B/other.jsonl"
a "A/widget" ctx > "$LAB/c1.out" 2> "$LAB/c1.err" || { cat "$LAB/c1.err"; fail "ctx sync failed"; }
[ -f "$CLAUDE_B/sess.jsonl" ] || { cat "$LAB/c1.err"; fail "A's session did not reach B"; }
[ -f "$CLAUDE_B/memory/MEMORY.md" ] || fail "nested memory/ dir did not reach B"
[ -f "$CLAUDE_A/other.jsonl" ] || fail "B's session did not reach A"
echo "PASS bidirectional, subdirectories included"

step "ctx: a second run must be a no-op (mtime carried across)"
a "A/widget" ctx > "$LAB/c2.out" 2> "$LAB/c2.err" || fail "second ctx failed"
grep -qE "0 pulled, 0 pushed" "$LAB/c2.err" \
  || { cat "$LAB/c2.err"; fail "sync did not converge — files are bouncing back and forth"; }
echo "PASS converged"

step "ctx: an append extends cleanly with no conflict copy"
printf '{"turn":1}\n{"turn":2}\n' > "$CLAUDE_A/sess.jsonl"
a "A/widget" ctx > "$LAB/c3.out" 2> "$LAB/c3.err" || fail "append sync failed"
[ "$(cat "$CLAUDE_B/sess.jsonl")" = "$(printf '{"turn":1}\n{"turn":2}')" ] \
  || fail "append did not land on B"
ls "$CLAUDE_B" | grep -q "conflict-" && fail "an append must not create a conflict copy"
echo "PASS appended, no conflict"

step "ctx: genuine divergence keeps both versions"
printf 'A-only content\n' > "$CLAUDE_A/sess.jsonl"
sleep 1
printf 'B-only content\n' > "$CLAUDE_B/sess.jsonl"
a "A/widget" ctx > "$LAB/c4.out" 2> "$LAB/c4.err" || fail "divergence sync failed"
grep -q "conflict" "$LAB/c4.err" || { cat "$LAB/c4.err"; fail "divergence was not reported"; }
[ "$(cat "$CLAUDE_A/sess.jsonl")" = "B-only content" ] || fail "newer side should have won"
ls "$CLAUDE_A" | grep -q "conflict-" || fail "the losing version was destroyed"
grep -rq "A-only content" "$CLAUDE_A" || fail "A's content was lost entirely"
echo "PASS newer won, loser preserved"

step "ctx: each project has its own context, not a shared one"
CLAUDE_A_G="$(claude_dir A gadget)"
mkdir -p "$CLAUDE_A_G"
printf 'gadget session\n' > "$CLAUDE_A_G/g.jsonl"
a "A/gadget" ctx > "$LAB/c6.out" 2> "$LAB/c6.err" || { cat "$LAB/c6.err"; fail "gadget ctx failed"; }
[ -f "$(claude_dir B gadget)/g.jsonl" ] || fail "gadget context did not reach B"
# widget's context must not have absorbed gadget's file.
[ -f "$CLAUDE_B/g.jsonl" ] && fail "gadget context leaked into widget"
echo "PASS per-project context"

step "ctx: --dry-run writes nothing"
printf 'pending\n' > "$CLAUDE_A/pending.jsonl"
a "A/widget" ctx --dry-run > "$LAB/c5.out" 2> "$LAB/c5.err" || fail "dry-run failed"
# Output is colourised, so match on the plain-text fragments only.
grep -q "pending.jsonl" "$LAB/c5.err" || { cat "$LAB/c5.err"; fail "dry-run did not plan the push"; }
grep -q "nothing written" "$LAB/c5.err" || fail "dry-run did not announce itself"
[ -f "$CLAUDE_B/pending.jsonl" ] && fail "dry-run wrote to the peer"
echo "PASS dry-run is inert"

step "ALL CHECKS PASSED"
