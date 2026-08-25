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
  # Every background process, not just B's node: a failure inside the watch step
  # would otherwise leave A's node and the watch alive, holding handles in $LAB
  # and answering later runs from a stale binary.
  for pid in "${NODE_PID:-}" "${A_NODE_PID:-}" "${WATCH_PID:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  for pid in "${NODE_PID:-}" "${A_NODE_PID:-}" "${WATCH_PID:-}"; do
    # The nodes hold file handles inside $LAB; removing it while a process is
    # still exiting fails with "device or resource busy" on Windows.
    [ -n "$pid" ] && wait "$pid" 2>/dev/null
  done
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
# Timestamps must carry a zone offset, i.e. they are local rather than UTC. This
# is a real regression guard, not decoration: reading the machine's timezone is
# only permitted while the process is single-threaded, so restoring
# `#[tokio::main]` — whose worker threads start before the body runs — makes the
# offset lookup fail *silently* and the log falls back to UTC on Unix while still
# working on Windows. Matching the format catches that; `+00:00` passes, since the
# question is whether the zone is known, not which zone it is.
grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}[+-][0-9]{2}:[0-9]{2} ' \
  "$LAB/node.log" \
  || { head -3 "$LAB/node.log"; fail "log timestamps are not local time with an offset"; }
echo ok

step "a second node on the same state directory is refused"
# Two nodes share the machine's identity and both register for it at the relay, so
# a request lands on whichever won the race while the other sits idle — possibly
# running older code, possibly holding a worktree mid-build.
OUT=$(b "$LAB" node 2>&1); RC=$?
[ "$RC" -ne 0 ] || { echo "$OUT"; fail "a second node started anyway"; }
grep -q "already running" <<<"$OUT" || { echo "$OUT"; fail "refusal does not say why"; }
# Named, so the reader knows which file to look at and that SW_HOME is the way to
# run a second node deliberately.
grep -q "node.lock" <<<"$OUT" || { echo "$OUT"; fail "refusal does not name the lock"; }
echo "PASS second node refused, exit $RC"

step "sw status works from outside a repo and reports both projects"
OUT=$(a "$LAB" status) || { echo "$OUT"; fail "status"; }
grep -q "widget" <<<"$OUT" || { echo "$OUT"; fail "status did not report widget"; }
grep -q "gadget" <<<"$OUT" || fail "status did not report gadget"
echo ok

# ------------------------------------------------- one node, two projects
step "verify BOTH projects over the single node and single pairing"
for name in widget gadget; do
  a "A/$name" verify > "$LAB/$name.out" 2> "$LAB/$name.err"
  [ $? -eq 0 ] || { tail -20 "$LAB/$name.err"; fail "$name verify did not return 0"; }
  grep -q "B/$name" "$LAB/$name.err" || { cat "$LAB/$name.err"; fail "$name built in the wrong checkout"; }
done
echo "PASS both projects verified"

step "a recipe added from the CLI runs by name — after it reaches the peer"
# `sw recipe add` writes the config; the peer executes ITS OWN copy of the
# recipe list (a patch cannot inject commands), so the new recipe must be
# committed and pulled before `sw verify hello` can find it there. This step
# locks in both halves: the CLI edit, and the documented propagation rule.
a "A/widget" recipe add hello "echo hello-from-recipe" > /dev/null 2>&1 || fail "recipe add"
a "A/widget" recipe | grep -q "hello-from-recipe" || fail "recipe list does not show it"
(cd "A/widget" && git add syncwork.toml && git commit --quiet -m "add hello recipe" \
   && git push --quiet origin HEAD)
(cd "B/widget" && git pull --quiet)
a "A/widget" verify hello > "$LAB/hello.out" 2> "$LAB/hello.err" \
  || { cat "$LAB/hello.err"; fail "verify hello failed"; }
grep -q "hello-from-recipe" "$LAB/hello.out" "$LAB/hello.err" 2>/dev/null \
  || { cat "$LAB/hello.err"; fail "recipe output did not come back"; }
echo "PASS CLI-added recipe ran on the peer"

# ------------------------------------- uncommitted error fails with diagnostic
step "the node records each request it served"
# The node streams stages and build output to the *requester*, so its own log is
# the only place the operator can see that a request arrived at all. Nothing else
# in this suite covers it: the banner is eprintln, the request lines are tracing,
# and only the banner was ever asserted on.
grep -q "verify widget/verify" "$LAB/node.log"   || { tail -20 "$LAB/node.log"; fail "node log has no record of the request"; }
grep -q -- "-> exit 0" "$LAB/node.log"   || { tail -20 "$LAB/node.log"; fail "node log has no record of the outcome"; }
# The path must be reported after the work, not at connect time: a connection is
# always relayed at the moment it opens, so sampling it there would claim every
# transfer went through the relay.
grep -q "served via" "$LAB/node.log"   || { tail -20 "$LAB/node.log"; fail "node log does not report the path it served over"; }
echo "PASS node log shows request and outcome"

step "verify outcomes land in the history file (both directions)"
# `sw tui` reads this back; a verify whose result is not recorded would leave
# the dashboard blind. A asked (sent), B built (served).
grep -q '"direction":"sent"' "$LAB/hA/history.jsonl"   || { cat "$LAB/hA/history.jsonl" 2>/dev/null; fail "requester recorded nothing"; }
grep -q '"direction":"served"' "$LAB/hB/history.jsonl"   || { cat "$LAB/hB/history.jsonl" 2>/dev/null; fail "builder recorded nothing"; }
grep -q '"project":"widget"' "$LAB/hA/history.jsonl" || fail "history entry lacks the project"
echo "PASS both sides remember the run"

step "the node records where the requester can be reached"
# A key is only dialable while some public directory is willing to translate it
# into an address, and that is not a safe assumption: n0's DNS was observed
# answering for one machine of a pair and returning NXDOMAIN for the other, whose
# record was present and current in the same service's own store. Half the pair
# went undialable for an outage neither machine could see. So each request states
# where its sender lives, and the receiver keeps it.
#
# B runs the node, so it is B's table that must have learned about A. A runs no
# node here, which exercises the slower path: a client discovering its own address
# during its first dial and writing it down for the commands that follow.
grep -q '^relay = ' "$LAB/hA/config.toml" \
  || { cat "$LAB/hA/config.toml"; fail "A never recorded its own address"; }
grep -A3 '^\[peers' "$LAB/hB/config.toml" | grep -q '^relay = ' \
  || { cat "$LAB/hB/config.toml"; fail "the node did not record where A can be reached"; }
grep -q "reachable at" "$LAB/node.log" \
  || { tail -20 "$LAB/node.log"; fail "address recorded but never logged"; }
echo "PASS both machines recorded where the other can be reached"

step "UNCOMMITTED compile error should fail with a located diagnostic"
cat > "A/widget/src/lib.rs" <<'EOF'
pub fn add(a: i32, b: i32) -> i32 { a + b }
pub fn broken() -> String { let x: i32 = "not an int"; x.to_string() }
EOF
a "A/widget" verify > "$LAB/v2.out" 2> "$LAB/v2.err"
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
a "A/widget" verify > "$LAB/v4.out" 2> "$LAB/v4.err"
[ $? -eq 0 ] || { tail -20 "$LAB/v4.err"; fail "fixed verify failed"; }
echo "PASS exit 0"

step "an unlinked project on the peer gives an actionable error"
b "$LAB" unlink gadget > /dev/null || fail "unlink"
a "A/gadget" verify > "$LAB/v5.out" 2> "$LAB/v5.err"
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

# ================================================================ sw handoff
step "handoff: fast-forwards a clean checkout on the peer"
# A new commit on A that B has never seen.
printf 'pub fn added() -> i32 { 7 }\n' >> "A/widget/src/lib.rs"
(cd "A/widget" && git add -A && git commit --quiet -m "add a function")
BEFORE="$(cd "B/widget" && git rev-parse --short HEAD)"
a "A/widget" handoff > "$LAB/h1.out" 2> "$LAB/h1.err" || { cat "$LAB/h1.err"; fail "handoff"; }
AFTER="$(cd "B/widget" && git rev-parse --short HEAD)"
[ "$BEFORE" != "$AFTER" ] || { cat "$LAB/h1.err"; fail "B did not advance"; }
grep -q "added()" "B/widget/src/lib.rs" || fail "the commit did not reach B's checkout"
echo "PASS $BEFORE -> $AFTER"

step "handoff: refuses to touch a dirty checkout, but still fetches"
printf 'pub fn more() -> i32 { 8 }\n' >> "A/widget/src/lib.rs"
(cd "A/widget" && git add -A && git commit --quiet -m "another")
# B has an uncommitted edit: its tree must not be disturbed.
printf 'scratch\n' > "B/widget/LOCAL_SCRATCH.txt"
(cd "B/widget" && git add LOCAL_SCRATCH.txt)
PINNED="$(cd "B/widget" && git rev-parse --short HEAD)"
a "A/widget" handoff > "$LAB/h2.out" 2> "$LAB/h2.err" || { cat "$LAB/h2.err"; fail "handoff should still succeed"; }
grep -q "uncommitted changes" "$LAB/h2.err" \
  || { cat "$LAB/h2.err"; fail "did not report why it declined"; }
[ "$(cd "B/widget" && git rev-parse --short HEAD)" = "$PINNED" ] \
  || fail "B's HEAD moved despite a dirty tree"
[ -f "B/widget/LOCAL_SCRATCH.txt" ] || fail "B's uncommitted file was destroyed"
# The commits must still have arrived, so a later pull is instant.
(cd "B/widget" && git cat-file -e "origin/master^{commit}" 2>/dev/null) \
  || (cd "B/widget" && git cat-file -e "origin/main^{commit}") \
  || fail "nothing was fetched"
echo "PASS declined safely, tree intact, commits fetched"

step "handoff: reports when a different branch is checked out there"
(cd "B/widget" && git reset --quiet HEAD LOCAL_SCRATCH.txt && rm -f LOCAL_SCRATCH.txt)
(cd "B/widget" && git switch --quiet -c sidetrack)
a "A/widget" handoff > "$LAB/h3.out" 2> "$LAB/h3.err" || { cat "$LAB/h3.err"; fail "handoff"; }
# Named, and named as being on the *other* machine: this text is composed there
# and printed here, so "here" would send you to check the wrong checkout.
grep -q "is checked out on node-b" "$LAB/h3.err" \
  || { cat "$LAB/h3.err"; fail "did not name the branch mismatch, and whose it is"; }
echo "PASS branch mismatch reported"

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

step "ctx: a declared root for a tool that is not Claude Code"
# The point of declared roots: another AI tool's state syncs by writing a rule into
# the committed config, with no code change and no per-machine setup. `repo` kind
# here because it is the one shape both machines resolve identically, so the test
# does not depend on either machine's home directory.
for m in A B; do
  (cd "$LAB/$m/gadget" && cat >> syncwork.toml <<'TOML'

[context.roots.othertool]
kind = "repo"
path = ".othertool"
TOML
  )
done
mkdir -p "$LAB/A/gadget/.othertool"
printf 'chat log\n' > "$LAB/A/gadget/.othertool/history.md"
a "A/gadget" ctx > "$LAB/c6.out" 2> "$LAB/c6.err" || { cat "$LAB/c6.err"; fail "ctx with a declared root failed"; }
[ -f "$LAB/B/gadget/.othertool/history.md" ] \
  || { cat "$LAB/c6.err"; fail "the declared root did not reach B"; }
grep -q "othertool" "$LAB/c6.err" || { cat "$LAB/c6.err"; fail "the root is not named in the report"; }
# And the built-in root is still synced alongside it, not replaced by it.
grep -q "claude" "$LAB/c6.err" || { cat "$LAB/c6.err"; fail "declaring a root dropped claude"; }
echo "PASS declared root synced, alongside the built-in one"

step "ctx: memory entries added on both machines merge, neither is lost"
# The incident this replays: both machines' Claude sessions append entries to
# MEMORY.md between syncs. Strict newer-wins would silently drop one side's
# memory; a conflict copy would demand manual merging every time. With a
# recorded common ancestor the two additions merge, and the merge result must
# converge on both machines with no conflict file.
MEM_A="$CLAUDE_A/memory"; MEM_B="$CLAUDE_B/memory"
mkdir -p "$MEM_A" "$MEM_B"
printf -- '- [Old](old.md) - existing entry\n' > "$MEM_A/MEMORY.md"
a "A/widget" ctx > /dev/null 2>&1 || fail "seed sync failed"
# Second run sees the file identical on both sides, which is what records the
# common ancestor on the initiating machine.
a "A/widget" ctx > /dev/null 2>&1 || fail "base-stamping sync failed"
[ -f "$MEM_B/MEMORY.md" ] || fail "seed did not reach B"

# Diverge: each machine appends its own entry. mtimes must differ from each
# other but both be past the quiet window, hence the touches below.
printf -- '- [FromA](a.md) - added on A\n' >> "$MEM_A/MEMORY.md"
printf -- '- [FromB](b.md) - added on B\n' >> "$MEM_B/MEMORY.md"
touch -t 202601010101 "$MEM_A/MEMORY.md"
touch -t 202601010202 "$MEM_B/MEMORY.md"

a "A/widget" ctx > "$LAB/m.out" 2> "$LAB/m.err" || { cat "$LAB/m.err"; fail "merge sync failed"; }
grep -q "merge" "$LAB/m.err" || { cat "$LAB/m.err"; fail "the divergence was not merged"; }
for side in "$MEM_A" "$MEM_B"; do
  grep -q "FromA" "$side/MEMORY.md" || { cat "$side/MEMORY.md"; fail "A's entry lost on $side"; }
  grep -q "FromB" "$side/MEMORY.md" || { cat "$side/MEMORY.md"; fail "B's entry lost on $side"; }
  grep -q "Old"   "$side/MEMORY.md" || fail "the pre-existing entry vanished on $side"
done
ls "$MEM_A" "$MEM_B" | grep -q "conflict" && fail "a clean merge still left a conflict copy"
# And the pair must converge: one more run moves nothing.
a "A/widget" ctx > /dev/null 2> "$LAB/m2.err" || fail "post-merge sync failed"
grep -qE "0 pulled, 0 pushed" "$LAB/m2.err" \
  || { cat "$LAB/m2.err"; fail "merge did not converge — files still bouncing"; }
echo "PASS both entries kept on both machines, no conflict copy, converged"

step "ctx --watch: picks up a change without being asked again"
# A's own node is started FIRST, so this step also proves the fix for a real
# incident: a client that dials with the machine key replaces the node's relay
# registration (the relay keeps one active connection per key), so a running
# `ctx --watch` on the Mac made the Mac undialable — `sw doctor` on Windows
# timed out against a node that was up the whole time. Clients therefore dial
# with throwaway keys and carry a machine-key delegation instead; with the
# watch running, A must remain reachable from B.
(cd "$LAB" && exec env SW_HOME="$LAB/hA" CLAUDE_CONFIG_DIR="$LAB/claudeA" "$SW" node \
   > "$LAB/a.node.log" 2>&1) &
A_NODE_PID=$!
for _ in $(seq 1 60); do
  grep -q "waiting for requests" "$LAB/a.node.log" 2>/dev/null && break
  sleep 0.5
done
grep -q "waiting for requests" "$LAB/a.node.log" || { cat "$LAB/a.node.log"; fail "A's node did not start"; }

# Left running in the background, as it would be in real use.
rm -f "$CLAUDE_A/pending.jsonl"
(cd "$LAB/A/widget" && exec env SW_HOME="$LAB/hA" CLAUDE_CONFIG_DIR="$LAB/claudeA" \
   "$SW" ctx --watch > "$LAB/w.out" 2> "$LAB/w.err") &
WATCH_PID=$!
for _ in $(seq 1 40); do
  grep -q "syncing on change" "$LAB/w.err" 2>/dev/null && break
  sleep 0.5
done
grep -q "syncing on change" "$LAB/w.err" || { cat "$LAB/w.err"; kill $WATCH_PID $A_NODE_PID 2>/dev/null; fail "watch did not start"; }

# The regression check itself: with A's watch connected to the relay, B dials A.
# Before the fix this timed out — the watch had replaced A's node at the relay.
OUT=$(b "$LAB" status 2>&1) \
  || { echo "$OUT"; kill $WATCH_PID $A_NODE_PID 2>/dev/null; fail "A became unreachable while its watch was running"; }
grep -q "up" <<<"$OUT" || { echo "$OUT"; kill $WATCH_PID $A_NODE_PID 2>/dev/null; fail "status did not reach A's node"; }
# A file appearing after the watch is already running is the whole point.
printf '{"turn":2}\n' > "$CLAUDE_A/watched.jsonl"
for _ in $(seq 1 40); do
  [ -f "$CLAUDE_B/watched.jsonl" ] && break
  sleep 0.5
done
kill $WATCH_PID $A_NODE_PID 2>/dev/null; wait $WATCH_PID $A_NODE_PID 2>/dev/null
[ -f "$CLAUDE_B/watched.jsonl" ] || { cat "$LAB/w.err"; fail "watch did not sync the new file"; }
# And it must stay quiet while nothing is happening, or an hours-long watch buries
# the lines that matter under one report per minute.
grep -q "in sync" "$LAB/w.err" && { cat "$LAB/w.err"; fail "watch narrates idle polls"; }
# Watch lines are stamped in the log's exact format — they arrive over hours, so
# *when* is part of the information, and matching the node log lets the two be
# read side by side. One-shot output must NOT have grown stamps (checked against
# the ctx runs captured earlier).
grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}[+-][0-9]{2}:[0-9]{2} ' \
  "$LAB/w.err" || { head -3 "$LAB/w.err"; fail "watch lines carry no log-format timestamp"; }
grep -qE '^[0-9]{4}-' "$LAB/c1.err" && { head -3 "$LAB/c1.err"; fail "one-shot ctx output grew timestamps"; }
echo "PASS watch synced a change, stamped like the log, quiet otherwise"

step "ALL CHECKS PASSED"
