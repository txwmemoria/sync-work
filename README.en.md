# sw

[中文](README.md) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Changelog](CHANGELOG.md) · [MIT](LICENSE)

`sw` lets two machines (Windows, macOS, or Linux) work on the same git project without the push–pull–rebuild loop, and without losing the local AI session when you change desks.

The machines are peers. Either side can start a request. On a LAN they find each other with mDNS. Across networks they use [iroh](https://github.com/n0-computer/iroh): QUIC, public-key authentication, hole punching, and a relay only when a direct path fails. The relay sees ciphertext, not your source or chat logs.

The full reference — every config key, the verify flow, troubleshooting, and the source map — is the [Chinese README](README.md). This page is enough to install it and use it.

## What you can ask it to do

| You want | Command |
|---|---|
| Compile and test the working tree I have right now, including uncommitted files, on the other machine | `sw verify` |
| Take the AI session and memory to the other machine | `sw ctx` |
| I committed; bring the other machine up to this branch | `sw handoff` |
| See why a peer is unreachable | `sw doctor` |
| Watch both machines, projects, and verify history | `sw tui` |

`sw verify` does not commit and does not touch the other machine's checkout. The build runs in a persistent worktree under `.sw/wt/`. Code travels as a patch. `syncwork.toml` does not: the peer runs the recipe from its own copy, so a recipe change has to be committed and fetched before it applies across machines. `sw handoff` fast-forwards only a clean checkout that is already on the requested branch. It never force-pushes and never merges.

## Requirements

- Rust 1.91 or newer (`iroh` 1.0's floor)
- git
- Outbound HTTPS on port 443 when the two machines are not on the same LAN

## Install, once per machine

```bash
cargo install --path . --locked
```

`--locked` matters. Without it, `cargo install` ignores `Cargo.lock` and the two machines can build different dependency sets.

Pair them once. Tickets contain a public key, a machine name, and a platform. No paths and no project names, so the pairing covers every project you add later.

```bash
# machine A
sw pair init

# machine B
sw pair join sw1:XXXX...

# machine A, with the ticket B printed
sw pair join sw1:YYYY...

sw pair list
```

Start the node and register it to launch at login. macOS uses launchd, Windows a `Run` key, Linux a systemd user unit. No administrator account.

```bash
sw service install
```

A machine only answers `sw verify` and `sw ctx` while its node is running. Install the service on every machine you want the other side to use.

## One project

```bash
# machine A, inside the repo
sw init
# commit syncwork.toml

# machine B, in its own clone, after git pull
sw link
```

Paths may differ. The project name in `syncwork.toml` is the only shared identifier. Each machine stores `name → local path` in `~/.syncwork/config.toml`, which is never sent.

To sync Claude Code sessions, run `sw hook install` in the project on both machines. Then `sw doctor` should report `all checks passed`.

## Day to day

```bash
sw verify                 # recipe named verify
sw verify check           # recipe named check
sw verify --peer mac      # more than one peer
sw ctx                    # both directions, this project
sw ctx --dry-run
sw ctx --watch            # keep syncing while a long session is open
sw handoff                # push, then fast-forward the peer when that is safe
sw status
sw tui                    # q or Esc to leave
```

Same session, one machine at a time. If both sides `/resume` the same Claude session, the files fork. Both copies are kept (`.conflict-<host>-<timestamp>` on the loser of an unmergeable file). Markdown memory files are three-way merged; only a same-line edit becomes a conflict.

## Config

Each repository has one `syncwork.toml`, committed to git. There is no multi-project file. A second project is a second repository with its own file and another `sw link`.

```toml
[project]
name = "myapp"
remote = "origin"

[recipes.verify]
steps = ["cargo build --release --all-targets", "cargo test --all"]

[recipes.check]
steps = ["cargo clippy --all-targets -- -D warnings"]

[context]
quiet_secs = 5

[context.roots.claude]
kind = "claude-project"
```

Steps run in a real shell: PowerShell on Windows, `/bin/sh` elsewhere. Platform blocks are `windows`, `macos`, and `linux`. A platform block with `steps` replaces the base steps; `env` is merged. `sw doctor` reports keys that TOML accepts but `sw` ignores, such as a platform written `mac` instead of `macos`.

Edit by hand or with `sw init`, `sw recipe`, and `sw root`. The commands keep comments.

## State on disk

| Path | What |
|---|---|
| `~/.syncwork/identity.key` | This machine's private key. The only secret. |
| `~/.syncwork/config.toml` | Name, paired peers, project registry |
| `<repo>/syncwork.toml` | Project name, recipes, context roots |
| `<repo>/.sw/wt/<peer>/` | The peer's build worktree. Safe to delete; the next verify recreates it |
| `~/.syncwork/history.jsonl` | Verify history shown by `sw tui` |

`SW_HOME` moves that state directory. `SW_LOG=debug` is the connectivity log.

## Security, short version

A connection is authenticated by the peer's public key. Unpaired peers are dropped. The machine key stays with the node; a client uses a one-connection ephemeral key and a signature over it. Pairing is the trust boundary: a paired machine runs your recipes as your user, in your checkout's worktree. Do not pair a machine you would not hand a shell.

The full model is in the Chinese README under 「安全模型」.

## Limits

- One writer per Claude session file.
- Windows has no keep-alive. The `Run` key starts the node at login; a crash waits for the next login or `sw service start`.
- Cross-network setup needs `*.relay.n0.iroh.link` on port 443. A LAN with working multicast does not.
- git-lfs object fetch on the peer has not been tested. Pointers themselves travel in the patch.

## Development

```bash
cargo test
cargo build && bash tests/e2e.sh
```

GitHub Actions runs `cargo test --locked` and then `cargo build --release --locked` on Linux x86_64, Windows x86_64, and macOS arm64. Each run keeps a zip of `sw` (or `sw.exe`) plus `LICENSE` for 14 days. A `v*` tag publishes those three archives on the Release. `tests/e2e.sh` stays local because it needs iroh discovery.

See [CONTRIBUTING.md](CONTRIBUTING.md).
