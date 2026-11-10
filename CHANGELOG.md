# 变更

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

## [Unreleased]

- GitHub Actions 在 Linux x86_64、Windows x86_64、macOS arm64 上测试并打包；`v*` 标签发布这三个 zip

## [0.1.0] - 2026-11-10

第一次公开。

- `sw verify`：把当前工作区（含未提交文件）送到对端的常驻 worktree 里按配方构建，退出码和诊断回到本机
- `sw ctx`：按项目同步 AI 会话和记忆；`.md` 可三方合并
- `sw handoff`：推送当前分支，对端只在干净且可快进时移动本地分支
- `sw pair`、`sw service`、`sw doctor`、`sw tui`
- 局域网 mDNS 直连；跨网络走 iroh 的打洞，失败时由中转转发密文
