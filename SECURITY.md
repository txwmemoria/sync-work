# 安全

[English](#english)

## 该保密的只有一把私钥

`~/.syncwork/identity.key` 是这台机器的身份。丢了它，已配对的机器都要重新配对。把它连同 `~/.syncwork/` 一起排除在备份、同步盘和 git 之外。

配对 ticket 含公钥、机器名和平台，不含路径和项目名。它仍然是一次授权，用完即止，不要贴进 issue 或聊天记录。

## 配对就是信任边界

连上的对端会在你的用户身份下执行 `syncwork.toml` 里的配方，工作目录是仓库里的 `.sw/wt/<peer>/`。配方由对端自己的配置决定，对方发来的 patch 改不了要执行的命令。但对方可以改脚本内容，而配方经常就是去跑那些脚本。所以只配对你愿意交给一个 shell 的机器。

中转节点只看见密文和连接元数据。密钥不出这两台机器。跨网络不通时，风险是连不上，不是内容被中转读走。

## 报告漏洞

不要开公开 issue 来描述可利用的问题，也不要附上私钥、ticket、会话记录或完整的 `sw doctor` 输出（里面有机器标识和中转地址）。

用 GitHub 的私密报告：仓库 **Security → Report a vulnerability**。

[https://github.com/txwmemoria/sync-work/security/advisories/new](https://github.com/txwmemoria/sync-work/security/advisories/new)

如果私密报告不可用，写信到仓库所有者在 GitHub 个人资料里留下的联系方式。

## English

`~/.syncwork/identity.key` is the only secret. Keep that directory out of git, sync folders, and backups. Pairing is the trust boundary: a paired machine runs your recipes as your user. Patches cannot replace the recipe, but they can change scripts the recipe executes. The relay carries ciphertext and connection metadata.

Report a vulnerability through [a private advisory](https://github.com/txwmemoria/sync-work/security/advisories/new), not a public issue. Do not include private keys, pairing tickets, session logs, or a full `sw doctor` transcript.
