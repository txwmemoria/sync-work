# sw

在两台机器（Windows / macOS）上开发同一个项目时，把中间那些来回的手工操作去掉。

## 它解决什么

**一、想知道代码在另一个平台上能不能编过、测试能不能过。**

现在的流程是：提交 → 推送 → 切到另一台机器 → 拉取 → 跑一遍构建脚本 → 看结果。**每验证一次都要走这一整套**，而且往往只是想确认一个几行的改动。

**二、换机器之后 AI 会话断了。**

Claude Code 的对话记录和记忆存在本地（`~/.claude/` 下），另一台机器上没有。代码能靠 git 过去，这些不能。

`sw` 把它们各变成一条命令：

| 你想做的 | 命令 |
|---|---|
| 让另一台机器编译并测试我**现在**的代码（不用提交） | `sw verify` |
| 把 AI 会话记录和记忆带到另一台机器 | `sw ctx` |
| 提交完了，让另一台机器跟上 | `sw handoff` |
| 哪里不对，一次查清 | `sw doctor` |

两台机器地位对等 —— 没有主机和从机，任意一方都能发起。**也不要求在同一个网络**：各自能上网就能互通（经公网中转，走 HTTPS），在同一局域网或 VPN 里时会自动尝试直连提速。

## `sw verify` 实际长什么样

下面是真实输出（机器名和路径已替换）：在 Windows 上敲命令，Mac 编译。

```
verify mac-mini  ci  f3f2730e553a + 1 file (659 B → 368 B)
  on mac-mini (macos) [90e2491]  /Users/me/projects/sync-work
  › reusing worktree
  › applying patch
  › [1/2] cargo build --release --all-targets
error[E0308]: mismatched types
 --> src/style.rs:106:18
    |
106 |     let n: i32 = "this is not an integer";
    |            ---   ^^^^^^^^^^^^^^^^^^^^^^^^ expected `i32`, found `&str`
fail mac-mini exit 1 in 1.6s (2 errors, 0 warnings)
```

三件事值得注意：

1. **这份改动没有 commit，也没有 push。** 当前工作区 —— 包括未暂存和未跟踪的文件 —— 被打成 patch 直接送到对端。
2. **Mac 编译器的诊断带着 `src/style.rs:106:18` 原样回到了你的终端**，不是一坨日志让你自己找。
3. **退出码是对端的**（这里是 1）。所以它能像任何本地构建命令一样接进 `&&` 链、git hook，或者交给 AI 工具驱动「改 → 远端验 → 读错误 → 再改」的闭环。

速度：首次（对端冷缓存）1m39s，之后 **1.6s** —— 对端保留持久 worktree，`target/` 缓存存活。1.6 秒是关键：它让远端验证能进内层开发循环，而不是感觉像 CI。

## 从零到能用

需要 Rust 1.91+（iroh 的下限）和 git。四步：前三步每对机器只做一次，第四步每个仓库一次。

### 1. 安装（两台机器都装）

```bash
cargo install --path . --locked
```

`--locked` 不能省：`cargo install` 默认忽略 `Cargo.lock` 重新解析依赖，不加的话两台机器可能装出不一样的依赖版本。

### 2. 配对（就这一次）

在任何目录跑，不需要在仓库里：

```bash
# 机器 A
sw pair init
#   → 打印 sw1:XXXX...，复制它

# 机器 B
sw pair join sw1:XXXX...
#   → 它会打印一个回执 ticket，复制回来

# 机器 A
sw pair join sw1:YYYY...

sw pair list      # 两边各自确认
```

ticket 里只有公钥、机器名和平台 —— 没有路径、没有项目名。所以这一次配对覆盖你之后所有的项目。

### 3. 开机自启（每台机器一次）

```bash
sw service install
```

注册用户级自启并立刻把节点拉起来 —— macOS 用 launchd、Windows 用 `Run` 注册表键、Linux 用 systemd user unit，都**不需要管理员权限**。想先看看它会做什么，加 `--print`（只显示，不执行）。

节点是接收方：另一台机器发来的 `sw verify` / `sw ctx` 由本机节点执行。简单说，**想让对方使唤这台机器，这台机器就得跑节点** —— 双向用就两台都装。

### 4. 接入仓库（每个项目一次）

```bash
# 机器 A，在项目里
cd ~/projects/myapp
sw init             # 逐项询问：项目名、远端、起手配方（rust/node/python/go/blank）
# 按需再调配方（sw recipe，或直接改文件），提交 syncwork.toml

# 机器 B，在同一项目的本地克隆里
cd ~/code/myapp && git pull
sw link             # 把「项目名 → 本机路径」记进本机注册表
```

两边路径可以完全不同 —— 项目靠 `syncwork.toml` 里的**项目名**对应，路径各自记在本机，从不发到网络上。发起方那侧其实可以省略 `sw link`（`verify` / `ctx` 会自动登记当前仓库）；对端不能省，漏了也不怕，报错会告诉你去哪台机器跑什么。

**再接第二个项目？在那个仓库里把这一步重做一遍，就完了。** 配对和节点都是机器级的：一个节点服务所有项目，`syncwork.toml` 每个仓库各有一份，互不相干。

想让 AI 上下文自动同步，在两台机器的项目目录里再各跑一次 `sw hook install`。最后两边各跑一次 `sw doctor`，应为 `all checks passed`。

```bash
sw projects         # 本机注册了哪些项目
sw unlink myapp     # 取消注册
```

## 日常使用

### `sw verify` —— 在对端验证当前工作区

```bash
sw verify              # 默认配方 ci
sw verify check        # 换配方
sw verify --base HEAD~3   # 换基准 commit
sw verify --peer mac   # 超过两台机器时指定目标
sw verify --push       # 顺手把分支推到远端（通常不需要）
```

对端在哪个分支、有没有未提交的改动，都**不影响 verify** —— 构建发生在独立的 worktree（`.sw/wt/`）里，对端自己的工作区永远不被碰。两台机器各自在不同分支上开发、各自一堆未提交改动，互相验证照常。

### `sw handoff` —— 分支交接

```bash
sw handoff              # push 当前分支 → 让对端 fetch → 能快进就快进
sw handoff --no-push    # 已经推过了，只让对端 fetch
sw verify && sw handoff # 验证通过才交接
```

**对端绝不猜。** 它总是 fetch，但只在四个条件全满足时才移动本地分支：请求的分支正是它当前所在的分支、工作区干净、远端 ref 存在、且是严格快进。任何其他情况都只 fetch 并说明原因：

```
  ok mac-mini fetched master at 938ce0e
  note the working tree on mac-mini has uncommitted changes
```

这不是错误 —— commit 已经到了对端，下次 pull 是瞬间的。永不 force，永不 merge，永不碰脏树。push 这一步不做全自动：那是「我决定发布」的决策点，WIP 想验证有 `sw verify`，不需要 push。

### `sw ctx` —— AI 上下文同步

```bash
sw ctx              # 双向同步（按当前项目）
sw ctx --dry-run    # 只看会动什么
sw ctx --watch      # 挂着不管，一有变化就同步
```

同步完在另一台机器上 `/resume` 就能接续这台的会话，memory 一并共享。每个项目的上下文独立，互不串台。

三种触发方式各管一段：

- **Stop hook**（`sw hook install`）—— 会话结束时自动同步，最关键的时刻。写入 `.claude/settings.local.json` 而非会提交的 `settings.json`（免得没装 `sw` 的同事踩到失败的 hook）；对端睡着静默跳过，被拒绝才报错。`sw hook status` / `uninstall` 管理
- **`--watch`** —— 长会话中途换机器，或让对端写的东西自动拉过来。每 2 秒查本地变化（只看文件名/大小/mtime），约每分钟无条件同步一次以拉取对端；空闲时一言不发，对端睡了不退出，每行带时间戳
- **手动 `sw ctx`** —— 剩下的所有情况

同步规则三条：谁的改动时间新，谁作准；覆盖前先比对内容 —— 旧文件只是新文件的开头一段的话（会话记录都是追加写，这是常态），直接覆盖，什么都不丢；只有内容真的分叉了，才把本地那份留成 `.conflict-<主机名>-<时间戳>` 备份。**任何情况下都不丢数据。** 另外，最近 5 秒还在被写入的文件这一轮先跳过，等写入停了再同。

**需要你自己遵守的只有一条**：同一个会话，同一时间只在一台机器上聊。两边同时 `/resume` 同一个会话必然分叉 —— 两份都会保住，但得你手工合并。

### `sw status` / `sw doctor`

```bash
sw status           # 对端在不在线、走直连还是 relay、它服务哪些项目
sw doctor           # 哪里不对一次查清（见「排查」）
```

## 更新

```bash
scripts/update.sh        # macOS / Linux
.\scripts\update.ps1     # Windows
```

脚本从自身路径定位仓库，在哪执行都行；顺序固定为**停节点 → git pull → cargo install → 起节点 → 跟日志**（Ctrl-C 只停止观看）。手动等价于：

```bash
sw service stop && git pull --ff-only && cargo install --path . --locked && sw service start
```

先停节点不是多余步骤：Windows 上不停根本装不上（运行中的 exe 被锁）；macOS 上更隐蔽 —— 装是装上了，节点却还在跑内存里的旧代码。协议版本变化时两台必须一起更新，混版本会明确报错。

```bash
sw service status | restart | stop | start | uninstall
sw service logs -f       # 节点日志，统一三个平台的位置差异（-n 行数，--path 只打印位置）
```

## 不只是写代码

`sw` 对「项目」的全部假设只有两条：在 git 里，以及产出或检验能写成命令。配方是任意 shell 命令，所以多设备协同的非代码项目一样适用 —— 笔记本上写 LaTeX 书稿、让台式机出 PDF（`sw recipe add pdf "latexmk -pdf main.tex"`）；改完博客让另一台构建全站；笔记本改分析脚本、让有 GPU 的机器试跑。这类场景往往是**单向**的：轻设备不用跑节点，只有干重活的那台跑（规则见「从零到能用」第 3 步）。AI 上下文同步同样与项目类型无关 —— 辅助写作的会话和写代码的会话是同一种文件。

## syncwork.toml 参考

**每个仓库一份**，放在仓库根目录、提交进 git，两台机器读到同一份。多项目不是在一份文件里写多个 `[project]`，而是每个仓库各写各的；名字到路径的映射由 `sw link` 记在各机自己的注册表里。

### 先看骨架：哪些字是你起的

整个文件只有四种段落。方括号里凡是 `<...>` 的位置，名字由你起、数量随意；其余全是固定键名：

```
[project]                      每份文件恰好一个
[recipes.<配方名>]              想加几个加几个；"ci" 只是 sw verify 的默认参数名
[recipes.<配方名>.<平台>]       可选的平台覆盖：windows / macos / linux
[context]                      至多一个
[context.roots.<根名>]          想加几个加几个
```

三个最常见的误读都出在这里：`ci` 和 `check` **不是保留字**，只是模板给的两个示例配方；「在一份文件里配置多个项目」**不存在**，一个仓库一份文件；`recipes.ci` **没有自动触发机制**，它只是 `sw verify` 不带参数时默认查找的名字。

### 全部的键，一张表查完

| 键 | 必填 | 默认 | 作用 |
|---|---|---|---|
| `project.name` | 是 | — | 两台机器认出「同一个项目」的名字，改了要两边一起改 |
| `project.remote` | 否 | `origin` | 对端缺 commit 时从这个远端 fetch |
| `recipes.<名>.steps` | 是 | — | 依次执行的 shell 命令，一条失败即停，退出码带回发起方 |
| `recipes.<名>.env` | 否 | 空 | 执行时注入的环境变量 |
| `recipes.<名>.cargo_json` | 否 | `true` | 给 cargo 命令注入 JSON 诊断输出（非 Rust 项目无影响） |
| `recipes.<名>.<平台>.steps` | 否 | — | 平台覆盖：**整体替换**基础 steps |
| `recipes.<名>.<平台>.env` | 否 | — | 平台覆盖：与基础 env **合并** |
| `context.quiet_secs` | 否 | `5` | 最近几秒内还在被写入的文件，本轮同步先跳过 |
| `context.roots.<名>.kind` | 是 | — | 三选一：`claude-project`（Claude Code 的按项目目录，路径自动推导）/ `home`（相对各机 home）/ `repo`（相对仓库目录） |
| `context.roots.<名>.path` | kind 为 home/repo 时必填 | — | 相对路径。禁止绝对路径 —— 一台机器的绝对路径在另一台上没有意义 |
| `context.roots.<名>.env` | 否 | — | 仅 home 可用：该环境变量有值时替换 home |

### 一份注释完整的示例

```toml
[project]
name = "myapp"
remote = "origin"

[recipes.ci]                # sw verify 默认跑它
steps = ["cargo build --release --all-targets", "cargo test --all"]

[recipes.check]             # sw verify check
steps = ["cargo clippy --all-targets -- -D warnings"]

[recipes.ci.windows]        # Windows 上 ci 换用这组 steps，env 则与基础合并
steps = ["cargo build --release", "cargo test --all -- --test-threads=1"]
env = { CC = "clang-cl" }

[context]
quiet_secs = 5

[context.roots.claude]      # Claude Code 会话与记忆
kind = "claude-project"

[context.roots.codex]       # 另一个 AI 工具：接入 = 写这几行，不改代码
kind = "home"
path = ".codex/sessions"

[context.roots.notes]       # 仓库里 git 不跟踪的目录也能同步
kind = "repo"
path = "docs/ai-notes"
```

### 不想手改文件：用命令搭

```bash
sw init                                      # 向导逐项询问（在脚本里跑则静默取默认值）
sw recipe                                    # 列出所有配方
sw recipe add e2e "bash tests/e2e.sh"        # 加/覆盖，跑法：sw verify e2e
sw recipe add ci "cargo test" --platform windows   # 给已有配方加平台覆盖
sw recipe remove e2e
sw root                                      # 列出根，并显示各自在本机指向哪个目录
sw root add notes --kind repo --path docs/ai
sw root remove notes
```

命令编辑会保留你手写的注释和排版，落盘前先校验。

### 三条行为规则

1. **step 在真实 shell 里跑**（Windows 是 PowerShell，macOS/Linux 是 `/bin/sh`），管道、重定向、调脚本都行 —— 但两平台 shell 语法有差异，跨平台命令写成单条（如用 `--manifest-path` 代替 `cd xx &&`），或干脆用平台覆盖。
2. **对端执行的是它自己那份配方**（发起方只能点名字，patch 注入不了命令）—— 所以改了配方要提交、对端拉取后才跨机生效。
3. **脚本文件的内容跟着 patch 走**（worktree 就是你当前的工作区）—— 把复杂逻辑放进脚本、配方只写一行调用，未提交的脚本改动也即时生效。

## 环境变量

| 变量 | 作用 |
|---|---|
| `SW_HOME` | 覆盖机器状态目录（默认 `~/.syncwork/`）。同机跑多个独立节点时用；设了它时 `sw reset` 不碰机器全局的自启项。 |
| `SW_LOG` | `tracing` 过滤器，默认 `error,sw=info`（不含依赖库警告）。排查连通性用 `SW_LOG=debug`。 |
| `SW_REMOTE` | 由 `sw` 注入配方步骤，值为 `1`，配方可用它区分远端执行。 |
| `NO_COLOR` | 关闭着色。非终端输出本来就不着色。 |
| `CLAUDE_CONFIG_DIR` | 覆盖 `~/.claude` 的位置，`sw ctx` 遵循。 |

## 设计

### 两层模型

| 存放位置 | 内容 | 作用域 |
|---|---|---|
| `~/.syncwork/config.toml` | 本机名字、已配对机器、**项目名 → 本机路径**注册表 | 机器 |
| `~/.syncwork/identity.key` | 私钥（唯一的秘密） | 机器 |
| `<仓库>/syncwork.toml`（进 git） | 项目名、配方、上下文声明 | 仓库 |

由此得到「配对一次、一台一个节点、自启装一次、提交的配置不含机器信息」。命令同样分两层：机器级（`pair` / `node` / `service` / `projects` / `status` / `doctor` / `reset`，在哪都能跑）和仓库级（`init` / `link` / `verify` / `ctx` / `handoff` / `hook`，从当前目录或 `-C` 发现仓库）。

**一台机器只允许一个节点**：第二个 `sw node` 会被文件锁（`~/.syncwork/node.lock`）挡下，并告诉你怎么处理 —— 两个节点共用一个身份，会在 relay 上互相顶替（机制见下面「安全模型」）。锁由内核随进程退出释放，不会留下残余；确实想并行跑独立节点，用 `SW_HOME`。

### 一次 verify 的完整流程

```
        Windows                                       Mac
+--------------------------+                 +--------------------------+
| sw verify  (in myapp)    |                 | sw node  (one / machine) |
|                          |                 |                          |
| 1 snapshot the worktree  |                 | 3 look up the registry:  |
|   (temp index; staging   |    iroh QUIC    |   myapp -> ~/code/myapp  |
|   area never touched)    |   pubkey auth,  | 4 checkout base,         |
| 2 diff -> zstd ----------+---encrypted---->|   apply the patch        |
|   + project name         |                 | 5 run the recipe in      |
|                          |                 |   .sw/wt/<peer>/         |
| 7 logs, diagnostics, <---+-----------------+ 6 stream every event     |
|   exit code              |                 |                          |
+--------------------------+                 +--------------------------+
```

**快照走临时 index**，绝不碰你真实的暂存区。**worktree 复用而不是每次重建**：对端在 `<仓库>/.sw/wt/<peer>/` 保留一个常驻 worktree，`target/` 缓存因此留得住 —— 1.6 秒就是这么来的；清理用 `git clean -fd`（特意没有 `-x`）。**对端缺 base commit 时自动回退**：它先 fetch，还是没有就回报，客户端改用与远端的公共祖先重新打一个更大的 patch 发过去 —— 所以只存在于本地的 commit 也不用先 push。**路径从不进网络**：请求里只有项目名，对端查自己的注册表、再核对项目名没错位；对端发来的相对路径逐段校验，出不了根目录。

### 安全模型

iroh 连接天生经过公钥认证 —— remote id 就是完成 QUIC 握手的那把公钥，「id 在本机对等表里」即是全部鉴权，没有 token 可泄漏或轮转；未配对的连接直接关闭。

但 **relay 对同一把 key 只保留一条活跃连接**，第二条连上来，第一条就被顶下线。客户端若拿机器密钥去连对端，等于把本机节点从 relay 上顶掉（`--watch` 曾因此让本机对外失联几个小时）。所以**机器密钥只归节点用**：客户端每次生成一把临时密钥来连接，请求里附上机器密钥对这把临时密钥的签名，对端验签通过后按机器授权。签名被偷走也没用 —— QUIC 握手已经证明了发送方手里有临时密钥的私钥。唯一的例外是配对：ticket 里带的就是机器公钥，必须由它亲自上线发布 —— 一次性操作，顶掉节点几秒钟无妨。

### 可达性：relay 牵线，打洞直连，地址自己记

**relay 是公网上的第三方**：iroh 的开发公司 n0, Inc. 在全球架设的免费公共节点，默认配置自动选延迟最近的一台（本项目两台机器实际用的是亚太节点 `aps1-1.relay.n0.iroh.link`）。交给它的只有元数据 —— 哪两把公钥在通信、何时、流量多大；**内容它看不到**：连接是端到端加密的 QUIC，patch、构建日志、会话记录全是密文，密钥从不离开你的两台机器。依赖它的风险是**可用性**而不是机密性（它的 DNS 服务出过一次事故，见下文的地址缓存）。不可接受时可以自建 —— `iroh-relay` 是开源的单个二进制，架在自己服务器上即可（见「路线图」）。

relay 身兼两职。一是**牵线**：两台机器都主动向外连到公网 relay（HTTPS/443，内网机器不需要端口映射），经它得知自己的公网出口地址、互换后同时向对方发包，把两边 NAT 上的洞凿开 —— 这就是打洞需要的那个「初始化服务器」。二是**兜底**：洞打不通就由 relay 转发。连接是端到端加密的，relay 只搬密文，密钥从不离开两台机器。一内网一公网是最容易的组合；双方都在 NAT 后则看 NAT 类型，打不通也还有 relay。

手里只有公钥的话，连不连得上就全看公共查询服务愿不愿意把 key 翻译成地址 —— 而这个假设被实测推翻过：n0 的权威 DNS 对一台机器正常应答，对另一台却返回「域名不存在」，记录明明就在同一个服务的存储里。所以每个请求都带上发送方自己的 relay 地址，接收方记进对等表（`sw doctor` 里显示为 `address`）。**只要通过一次，这对机器就再也不用求公共查询了**；连接时缓存地址和公共查询同时进行，地址过期顶多浪费一次探测。地址由节点在上线后写下，短命令直接从配置里读。

### 路径推导（ctx）

Claude Code 用项目的绝对路径给状态目录起名，`[A-Za-z0-9-]` 之外的字符一律换成 `-`（`D:\projects\sync-work` → `D--projects-sync-work`）。两台机器算出来的目录名必然不同 —— 所以各自用本地仓库的路径推导自己的目录名，网络上只传根的名字和相对路径。

## 排查

先跑这个：

```bash
sw doctor
```

本机身份、状态目录、地址、节点、自启，每个项目的路径，每个对端的可达性、密钥、构建版本 —— 一次全查。有失败项退出 1，只有警告退出 0，因为「另一台机器睡着了」不算配置错误。

```
this machine
  ✓ identity    0a1b2c3d4e5f6071…
  ✓ state       C:\Users\me\.syncwork
  ✓ build       0f3d4b0
  ✓ address     https://aps1-1.relay.n0.iroh.link./
  ✓ node        running
  ✓ autostart   registered

peers
  ✓ mac-mini  reachable
      relay aps1-1.relay.n0.iroh.link. [5.223.65.62] 357ms, *ip:10.0.0.23:55042 6ms
  ✓ address     https://aps1-1.relay.n0.iroh.link./ (cached)
  ✓ keys        agree (the peer accepted this machine's key)
  ✓ build       0f3d4b0 on both
```

常见结论的解释：

**`the peer refused us as unpaired`** —— 对端连上了但不认识你的公钥，通常因为本机身份被重新生成过。两边 `sw pair list` 对比 `id`，不一致就重新配对。

**`no address for peer ...`** —— 本机没存过它的地址，公共查询也没给出答案。**这不是配置错误，重新配对也没用。** 去那台机器上跑一次 `sw doctor` 就好 —— 它发请求过来时，地址就被记下了。

**`could not reach peer ...; is 'sw node' running there?`** —— 有地址但连不上：对端节点没在跑，或网络不通。`SW_LOG=debug sw status` 看握手细节。

**两个 shell 报告不同的 `id`** —— `sw pair list` 会打印实际使用的状态目录，先对比这个。配对要在你日常使用的那个 shell 里做。

想从零重来：`sw reset`（`--yes` 跳过确认）。只删本机自己的东西 —— 不碰仓库的 `syncwork.toml`、`.sw/` 缓存、`~/.claude`。删除前列出将失去什么，并提醒本机换新身份后**两边都要重新配对**。

## 测试与源码

```bash
cargo test          # 57 个单元测试
bash tests/e2e.sh   # 端到端：两台机器、两个项目、一个节点，27 项检查
```

端到端覆盖单元测试到不了的部分（配对、传输、worktree 隔离、上下文同步、watch、节点锁），其中多项是真实事故的回放。

```
src/
├── main.rs           入口：时区捕获、日志、着色、分发
├── cli.rs            命令行定义（只有定义，没有实现）
├── commands/         本地事务性命令：pair / project / hook / doctor / reset
├── config/           两层配置：machine（身份、对等表、注册表）+ repo（syncwork.toml）+ platform
├── transport/        线路层：message（协议类型）/ frame（分帧）/ endpoint（绑定、连接、鉴权）/ ticket
├── node.rs           常驻接受循环；只做路由
├── verify/           远端构建验证：client + server + exec（配方执行）
├── ctx/              上下文同步：roots（声明求值）/ scan / merge / client / server
├── git.rs            git 管道：快照、worktree、patch
├── service.rs        开机自启
└── style.rs          终端着色与时间戳，非终端时关闭
```

`verify/` 和 `ctx/` 各自同时拥有自己的两端（发起方和节点方）—— 改动任一侧的交互几乎必然同时动两端，分开放是在邀请漂移。`node.rs` 只是路由器。没有为「设计模式」而加的 trait 抽象。

## 已验证

在真实的 Windows 11 ↔ macOS 组合上跑通，不是模拟：双向构建验证（含未提交改动的诊断回传与退出码透传）、NAT 打洞直连（`*direct 6ms`，relay 兜底）、`sw ctx` 双向同步与对端 `/resume` 接续、`--watch` 挂着时本机仍可达、两平台开机自启与日志。

## 已知限制

- **同一个会话文件同一时刻只能一台机器写。** 两边同时 `/resume` 同一个会话必然分叉；两份都保住（`.conflict-*`），合并要手工。
- **Windows 没有 keep-alive。** `Run` 键只在登录时启动一次；节点崩了要等下次登录或手动 `sw service start`。
- **缓存地址在两台机器都换网时会双双过期**，那时只能等公共查询恢复或重新配对。
- **建立连接仍依赖 relay 可达**（`*.relay.n0.iroh.link` 的 443 端口）。缓存地址解决「查不到 key」，不解决「连不上」—— 网络出口若封锁这个域名就不通。
- **沙箱化终端可能读到不同的配置。** 配对要在日常使用的 shell 里做。
- **git-lfs 大资产仓库未实测。** patch 机制对 LFS 指针文件本身有效，但对端补取 LFS 对象的链路没验证过。

## 路线图

- **自建 relay**：把对 n0 公共设施的依赖变成可选项。服务端跑开源的 `iroh-relay`（单个二进制，内置 ACME 自动证书，需要一个域名和 443 端口）；`sw` 这边加一个机器级配置项指向它，配对 ticket 顺带携带 relay 地址，从此连公共查询的首次引导也不需要 —— 私有部署闭环。地址缓存机制已经就位，这一步是水到渠成
- `sw tui` — ratatui 面板，机器状态、项目列表与构建历史
- 超过两台机器时，一条命令让所有机器同时验证（`sw verify --all`）
- Windows keep-alive（不用管理员权限的前提下）
- `sw mcp` — 以 MCP 服务暴露 verify / handoff / ctx / status，给没有 shell 权限的 AI 宿主（如 ChatGPT 桌面版）用，并返回结构化 JSON 而非终端文本。有 shell 的工具（Claude Code）直接跑命令已经够好，所以等真在无 shell 宿主里需要时再做
