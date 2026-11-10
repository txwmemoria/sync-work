# 贡献

[English](#english)

用户手册在 [README.md](README.md)，英文入口在 [README.en.md](README.en.md)。这里只写怎么改这份代码。

## 环境

- Rust 1.91 或更新。`Cargo.toml` 的 `rust-version` 就是下限，再低编不过 `iroh` 1.0。
- git。`build.rs` 用它给二进制盖提交号；没有 git 元数据时编译仍会成功，版本戳是 `unknown`。

```bash
cargo test
cargo build
bash tests/e2e.sh
```

`cargo test` 不联网。`tests/e2e.sh` 在本机用两个 `SW_HOME` 扮两台机器，要先有 debug 二进制，并且出网能到 iroh 的发现服务。它会自己清掉临时目录和后台节点。

推到 GitHub 后，Actions 会在 Linux、Windows、macOS 上跑 `cargo test --locked` 和 release 编译。安装包在运行页面的 artifact 里。要发正式的三端 zip，推一个 `v*` 标签，例如 `git tag v0.1.0 && git push origin v0.1.0`。标签工作流会再测一次再挂到 Release。

安装到本机用 `cargo install --path . --locked`，和 README 里给用户的命令一致。不要省略 `--locked`。

## 改哪里

命令行只有定义，在 `src/cli.rs`。实现按职责拆开：

- 机器级命令（`pair`、`service`、`doctor`、`reset`、`tui`）在 `src/commands/`
- `verify` 的两端都在 `src/verify/`，`ctx` 的两端都在 `src/ctx/`
- `src/node.rs` 只做路由

改 verify 或 ctx 的线协议时，发起方和节点方一起改。协议版本不一致时，两端会直接报错，不要做静默兼容。

`syncwork.toml` 的未知键故意被解析器留下，好让新旧版本读同一份文件。新增会被忽略的键时，让 `sw doctor` 把它喊出来。

## 不要提交的东西

- `~/.syncwork/`，尤其是 `identity.key`
- 配对 ticket、会话记录、`.conflict-*` 备份
- `target/`、`.sw/`

## Pull request

说明为什么改，以及你跑过 `cargo test` 还是也跑过 `tests/e2e.sh`。行为变化写进 [CHANGELOG.md](CHANGELOG.md) 的 `Unreleased`。

## English

The user manual is [README.en.md](README.en.md). This file is how to change the code.

You need Rust 1.91+ and git. `cargo test` is offline. `bash tests/e2e.sh` needs `cargo build` first and a network path to iroh discovery; it simulates two machines on one host with separate `SW_HOME` directories.

CI repeats `cargo test --locked` and the release build on Linux, Windows, and macOS. Pushing a `v*` tag publishes the three zip archives.

Keep both ends of a protocol change in `src/verify/` or `src/ctx/` together. Do not commit `~/.syncwork/`, pairing tickets, or session logs. Note behavior changes under `Unreleased` in [CHANGELOG.md](CHANGELOG.md).
