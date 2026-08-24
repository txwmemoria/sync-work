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

两台机器地位对等 —— 没有主机和从机，任意一方都能发起。

## `sw verify` 实际长什么样

下面是真实输出：在 Windows 上敲命令，Mac 编译。

```
verify niugms-macbook-pro-local  ci  f3f2730e553a + 1 file (659 B → 368 B)
  on niugms-macbook-pro-local (macos) [90e2491]  /Users/niugm/projects/sync-work
  › reusing worktree
  › applying patch
  › [1/2] cargo build --release --all-targets
error[E0308]: mismatched types
 --> src/style.rs:106:18
    |
106 |     let n: i32 = "this is not an integer";
    |            ---   ^^^^^^^^^^^^^^^^^^^^^^^^ expected `i32`, found `&str`
fail niugms-macbook-pro-local exit 1 in 1.6s (2 errors, 0 warnings)
```

三件事值得注意：

1. **这份改动没有 commit，也没有 push。** 当前工作区 —— 包括未暂存和未跟踪的文件 —— 被打成 patch 直接送到对端。
2. **Mac 编译器的诊断带着 `src/style.rs:106:18` 原样回到了你的终端**，不是一坨日志让你自己找。
3. **退出码是对端的**（这里是 1）。所以它能像任何本地构建命令一样接进 `&&` 链、git hook，或者交给 AI 工具驱动「改 → 远端验 → 读错误 → 再改」的闭环 —— 不需要人把失败信息转述一遍。

速度：

| | 耗时 |
|---|---|
| 首次（对端冷缓存） | 1m39s |
| 之后 | **1.6s** |

差别来自对端保留了一个持久 worktree，`target/` 缓存不会被清掉。1.6 秒这个数字是关键 —— 它让远端验证能进内层开发循环，而不是感觉像 CI。

## 快速上手

```bash
cargo install --path . --locked     # 两台机器都装
sw pair init                        # 一台机器上，一次性
sw pair join <ticket>               # 另一台机器上
sw service install                  # 两台各一次，节点开机自启
```

然后在每个想跨平台验证的项目里：`sw init`（第一台）、`sw link`（另一台）。细节见下面「一次性设置」和「接入一个仓库」。

## 两层模型（先看这个）

配置分两层，这个划分决定了整个工具的形状：

| 存放位置 | 内容 | 作用域 |
|---|---|---|
| 机器状态目录下的 `config.toml` | 本机名字、已配对的机器、**项目名 → 本机路径** 的注册表 | 机器 |
| 机器状态目录下的 `identity.key` | 私钥（唯一的秘密） | 机器 |
| `<仓库>/syncwork.toml`（进 git） | 项目名、构建配方、上下文选项 | 仓库 |

机器状态目录是 `~/.syncwork/`，三个平台一致（`SW_HOME` 可覆盖）。

放在用户目录而不是各平台的配置目录，是因为**沙箱化的应用会让后者被重定向**。实测：Claude 桌面版是打包应用，它对 `%APPDATA%` 的读写落在 `AppData\Local\Packages\...\LocalCache\Roaming` 下，而且写死绝对路径也逃不出去 —— 重定向发生在路径解析层。结果是同一台机器上出现两个身份，其中一个对端从未配对过，于是从那个应用里跑的任何 `sw` 都被拒绝。`~/.claude` 实测**不**被重定向（两边看到同一批会话记录），所以状态就放在它旁边。

旧位置的状态会在首次运行时自动迁移过来；迁移失败则继续用旧位置，不会因为搬家失败而弄坏一个能用的配对。

由此得到：

- **两台机器只配对一次**，不是每个仓库配一次。
- **每台机器只跑一个 `sw node`**，服务它注册的所有项目。
- **只装一次开机自启**，加新项目不用碰服务。
- 仓库里提交的 `syncwork.toml` **不含任何机器信息**，所以两边读到的完全一样。

项目靠 `syncwork.toml` 里的**项目名**跨机器对应。路径由每台机器各自记录 —— 绝对路径从不上网络。

命令也分两层：

- **机器级**（在任何目录都能跑，不需要在仓库里）：`pair`、`node`、`service`、`projects`、`status`
- **仓库级**（从当前目录或 `-C` 发现仓库）：`init`、`link`、`verify`、`ctx`

## `sw node` 是什么，谁需要跑它

`sw node` 是**接收方**。它常驻监听，把已配对机器发来的请求在本机执行：应用 patch、在 worktree 里跑配方、收发上下文文件。

`sw verify` 和 `sw ctx` 是**发起方**，它们是一次性命令，**不需要本机有节点在跑**。

所以规则很简单：**一台机器需要 `sw node`，当且仅当你希望另一台机器能请它干活。**

| 你想要的 | 谁需要 `sw node` |
|---|---|
| 只在 Windows 开发，让 Mac 验证 | 只有 Mac |
| 只在 Mac 开发，让 Windows 验证 | 只有 Windows |
| **双向交叉验证**（两边都能发起） | **两台都要** |
| 从任一侧 `sw ctx` 同步上下文 | 对端要 |

因为设计上没有主机概念、任意一方都能发起，**两台都跑节点**才能真正双向。这也是 `sw service install` 存在的理由 —— 两台机器各装一次，之后不用再记得手动启动。

节点和客户端互不干扰：一台机器可以同时跑着 `sw node` 服务对端，又用 `sw verify` 请对端干活。

**一台机器只允许一个节点。** 第二个 `sw node` 会被拒绝并告诉你锁在哪：

```
error: another `sw node` is already running on this machine — it holds
       C:\Users\sdkpo\.syncwork\node.lock. Stop it with `sw service stop`, ...
```

不是洁癖 —— 两个节点共用同一个身份、都在 relay 上注册它，请求会落到抢赢的那个，另一个空转：可能跑着旧代码，也可能正握着一个 worktree。用的是操作系统文件锁而不是 pid 文件，所以进程无论怎么死锁都会被内核释放，没有残留状态要清理。真想同时跑两个独立节点，用 `SW_HOME` 给它们各自的状态目录（端到端测试就靠这个）。

## 安装

两台机器都要装。需要 Rust 1.91+（iroh 的下限）和 git。

```bash
cargo install --path . --locked
```

`--locked` 不是可选的讲究：`cargo install` **默认会忽略 `Cargo.lock` 重新解析依赖**，两台机器装出来的依赖版本可能不同。加上它才保证双端构建一致。

或直接用 `target/release/sw`（单文件，无运行时依赖）。

## 更新

```bash
git pull && cargo install --path . --locked
```

`cargo install --path .` 直接替换已安装的二进制 —— 不需要 `--force`，版本号不变也会替换。

**但更新完必须重启节点**，运行中的 `sw node` 用的是已加载进内存的旧代码。两个平台的失败方式不同，后者更危险：

| 平台 | 节点正在运行时执行 install |
|---|---|
| Windows | **失败**：`failed to remove file ...sw.exe`（运行中的 exe 被文件锁占用） |
| macOS / Linux | **成功**，但运行中的节点仍跑旧代码 —— 静默过期 |

Windows 会当场报错，你不会搞错。POSIX 允许替换正在运行的可执行文件，所以 macOS 上 install 会显示成功，而节点行为毫无变化 —— 看起来像"更新没生效"。

所以顺序固定为：**先停节点 → install → 重启节点**。

### 一键脚本

装了服务之后，更新用脚本就够了 —— 拉取、编译安装、重启节点、跟踪日志，顺序已经排好：

```bash
scripts/update.sh
```

```powershell
.\scripts\update.ps1
```

脚本从**自身路径**定位仓库，所以在任何目录执行都可以。它**先停节点再编译** —— 这个顺序在 Windows 上是强制的（`cargo install` 要替换的正是节点在跑的那个 exe，运行中无法替换），在 macOS 上则是为了避免"装成功了但行为没变"。最后一步跟踪日志，Ctrl-C 只停止观看，节点继续运行。

手动等价于：

```bash
sw service stop && git pull --ff-only && cargo install --path . --locked && sw service start
```

前台手动跑节点的话，先在那个终端 Ctrl-C，再 install，再重新 `sw node`。

## 一次性设置

### 1. 配对两台机器（就这一次）

在任何目录跑，不需要在仓库里：

```bash
# 机器 A
sw pair init
#   → 打印 sw1:XXXX...，复制它

# 机器 B
sw pair join sw1:XXXX...
#   → 打印另一个 ticket，复制回来

# 机器 A
sw pair join sw1:YYYY...
```

ticket 里只有公钥、机器名和平台 —— 没有路径、没有项目名。所以这一次配对覆盖你之后所有的项目。

```bash
sw pair list      # 确认
```

### 2. 装开机自启（每台机器一次）

```bash
sw service install           # 注册开机自启
sw service install --print   # 先看它会做什么，不实际执行
sw service status            # 是否已注册
sw service restart           # 更新二进制后用它生效
sw service stop / start      # 分开的两半 —— 更新脚本用 stop 先于编译
sw service logs -f           # 跟踪节点日志（平台差异由它处理）
sw service uninstall
```

`sw service logs` 省掉了记三个不同答案的麻烦：macOS 是 `~/Library/Logs/` 下的文件，Windows 是 `%LOCALAPPDATA%\syncwork\` 下的文件，Linux 根本不是文件而是 systemd journal。`-f` 跟踪，`-n` 指定回看行数。`--path` 只打印位置，不打开跟踪（脚本里想避免占住文件句柄时用）。

Windows 上节点自己写日志文件（`sw node --log <path>`），而不是让 PowerShell 重定向。两个原因：PowerShell 的 `*>` 写的是 UTF-16 并把 stderr 每行包成 `NativeCommandError`，日志既不可读也 grep 不到；更要紧的是**被重定向的子进程会继承父进程的句柄**，于是节点会一直握着调用方的管道不放 —— 任何读 `sw service start` 输出的脚本都会永久卡住。节点自己开文件就同时解决了这两件事，而且是追加而非截断，重启后上一轮的日志还在。

**更新完一定要 `sw service restart`。** `install` 不等于重启：它只保证「下次登录会启动新二进制」，不会替换当前正在跑的那个进程。

| 平台 | 机制 |
|---|---|
| macOS | launchd LaunchAgent（`~/Library/LaunchAgents`） |
| Windows | `HKCU\...\CurrentVersion\Run` 注册表值，指向一个隐藏窗口的 `.ps1` 启动脚本 |
| Linux | systemd user unit |

用户级，全都不需要管理员权限。

Windows 用的是 `Run` 注册表键而不是计划任务：在常见的企业策略下创建带登录触发器的任务会被直接拒绝 —— `schtasks /Create /SC ONLOGON` 返回 `Access is denied`，加 `/RU` `/IT` 也一样。`Run` 键不需要提权。代价是 Windows 上没有守护者，所以 `install` 和 `restart` 会自己把节点拉起来，也没有 keep-alive（macOS 的 launchd 有 `KeepAlive`，Linux 的 systemd 有 `Restart=always`）。

安装时会把**当前的 PATH 写进服务定义**：这三种机制启动进程时环境都最小化，一个能启动但找不到 `cargo` 的节点比启动失败更糟 —— 对端连得上，然后报 `cargo: not found`，看起来像项目坏了。

调试时也可以前台跑：`sw node`。

## 接入一个仓库

对每个想跨平台验证的项目做一次。

```bash
# 机器 A，在项目里
cd ~/projects/myapp
sw init
```

生成 `syncwork.toml` 并把 `.sw/` 加进 `.gitignore`。按需改配方，然后提交：

```toml
[project]
name = "myapp"
remote = "origin"

[recipes.ci]
steps = ["cargo build --release --all-targets", "cargo test --all"]

[recipes.ci.windows]        # 只在需要时覆盖平台差异
steps = ["cargo build --release", "cargo test --all -- --test-threads=1"]
env = { CC = "clang-cl" }

[context]
quiet_secs = 5

[context.roots.claude]      # 要同步哪些 AI 状态目录，见「AI 上下文同步」
kind = "claude-project"
```

```bash
# 机器 B，在同一项目的检出里
cd ~/code/myapp && git pull
sw link
```

`sw link` 把「项目名 → 本机路径」记进机器注册表。两边路径可以完全不同，这正是它记在本机而不是仓库里的原因。

```bash
sw projects      # 看本机注册了哪些
sw unlink myapp  # 取消
```

配方提交进 git，两台机器读同一份 —— 这是「双端执行相同脚本」的保证，而不是靠人手工对齐。支持的平台覆盖键：`windows` / `macos` / `linux`；覆盖块里 `steps` 整体替换，`env` 与基础配置合并。

## 日常使用

```bash
sw verify              # 用默认配方 ci 在对端验证当前工作区
sw verify check        # 换配方
sw verify --peer mac   # 超过两台机器时指定目标
sw verify --base HEAD~3   # 换基准 commit
sw verify --push       # 顺手把分支推到 GitLab（通常不需要）
sw status              # 对端是否在线、它服务哪些项目
```

## 分支交接

commit 完想换到另一台机器继续，原本是「这边 push、那边 pull」两步。一条命令：

```bash
sw handoff              # push 当前分支 → 让对端 fetch → 能快进就快进
sw handoff --no-push    # 已经推过了，只让对端 fetch
```

配合 verify 就是完整的交接：

```bash
sw verify && sw handoff
```

**为什么不做成全自动**（比如 commit 时自动 push）：那一步不是传输，是「我决定发布」的决策点。WIP 想在对端验证，`sw verify` 已经覆盖了，不需要 push。真正需要 push 的时候是你判断这些 commit 该进远端 —— 而推出去的历史再改很痛。

**对端绝不猜。** 它总是 fetch，但只在四个条件全满足时才动检出：请求的分支正是它检出的那个、工作区干净、远端 ref 存在、且是严格快进。任何其他情况都只 fetch 并说明原因：

```
  ok niugms-macbook-pro-local fetched master at 938ce0e
  note the working tree here has uncommitted changes
```

这不是错误 —— commit 已经到了对端，下次 pull 是瞬间的。对端拒绝移动一个它无法安全移动的检出，正是预期行为；**说清为什么**才是价值所在。永不 force，永不产生 merge commit，永不碰脏树。

`verify` 和 `ctx` 会自动把当前仓库登记到本机注册表，所以发起方那侧不必先手动 `sw link`。对端仍然需要 —— 如果没做，你会看到：

```
error: project `myapp` is not linked on mac-mini — run `sw link` in your clone of it there
```

## AI 上下文同步

代码走 git，但 AI 工具在仓库**之外**保存的东西不会走 —— Claude Code 的会话记录和 memory 都在 `~/.claude/projects/` 下。换台机器，上下文就没了。

```bash
sw ctx              # 双向同步（按当前项目）
sw ctx --dry-run    # 只看会动什么
sw ctx --watch      # 留着不管，本地一变就同步
```

同步完在另一台机器上 `/resume` 就能看到这台的会话，memory 也一并共享。每个项目的上下文独立，互不串台。

`--watch` 和下面的 Stop hook 是互补的，不是替代：hook 管「会话结束了」这个最关键的时刻；`--watch` 管剩下的 —— 一个长会话中途想换机器接着聊，以及**把对端写的东西拉过来**而不用记得去要。

它每 2 秒看一眼本地根目录（只看文件名、大小、mtime，不读内容，所以这个频率不心疼），变了就同步；另外每约 60 秒无条件同步一次，否则只盯本地就变成单向了。没有东西动的时候它一句话都不说 —— 一个挂几小时的进程要是每分钟报一次「0 pulled, 0 pushed」，真正要紧的行就全被埋了。它也不会因为对端睡了就退出（`--watch` 自带 `--if-reachable`）。

watch 模式下每行输出带时间戳，格式与节点日志**完全一致**（同一个格式定义，不是两份恰好相同的字符串）—— 挂几小时的输出，「什么时候发生的」本身就是信息，而且格式一致意味着 watch 的一行和节点日志里同一次同步的一行可以并排对时间。一次性 `sw ctx` 不加时间戳：一秒跑完的输出全发生在「现在」。

### 让它自动跑

不想每次都记得，就挂上 Claude Code 的 Stop hook —— 会话结束时自动同步：

```bash
sw hook install     # 两台机器各自的检出里都要跑一次
sw hook status
sw hook uninstall
```

时机正好：会话结束意味着 transcript 停止增长，`quiet_secs` 的跳过规则不会碍事。

写入的是 `.claude/settings.local.json` 而**不是** `settings.json` —— 后者会提交，那样没装 `sw` 的同事每次会话结束都会踩到一个失败的 hook。命令里带绝对 `-C <仓库>`，因为 hook 的工作目录不保证。

hook 用的是 `sw ctx --if-reachable`：对端离线时静默退出 0。这个区分是刻意的 —— **不可达**是暂时状态（另一台机器睡着了），你无从下手；而**被拒绝**是配置错误，仍然会报错。

### 同步哪些目录（不止 Claude）

哪些目录算「上下文」是**声明**在 `syncwork.toml` 里的，不写死在代码里。因为 AI 工具不只一个，而它们唯一的共同点就是状态都在仓库之外、所以都不会走 git。

```toml
[context]
quiet_secs = 5              # 这么久内被改过的文件跳过（可能正被活跃会话写入）

[context.roots.claude]
kind = "claude-project"     # Claude Code 的按项目目录，见下面「路径推导」

[context.roots.codex]
kind = "home"               # 相对**各自机器**的 home
path = ".codex/sessions"

[context.roots.notes]
kind = "repo"               # 相对检出目录，用于 git 不跟踪的状态
path = "docs/ai-notes"
```

三种 `kind` 都是**规则**而不是路径 —— 两台机器各自在本地求值，线上只传根的**名字**。这是刻意的：一台机器上的绝对路径拿到另一台毫无意义。所以 `syncwork.toml` 提交进 git，两边读同一份，不需要在每台机器上分别配一遍再想办法保持一致。

`kind = "home"` 还可以加 `env = "SOME_HOME"`：那个环境变量有值时替换掉 home。`claude-project` 内部就是这么处理 `CLAUDE_CONFIG_DIR` 的。

加一个新工具不需要改代码，写四行配置就行。**注意**：根必须是**目录**。有些工具把状态存在单个**文件**里（Aider 的 `.aider.chat.history.md`），指过去会有告警且同步不到东西 —— 那种情况指它所在的目录。

老的 `claude = true` / `extra = [...]` 写法仍然读得懂并自动折算，不用改现有仓库。

### 路径推导

Claude Code 按项目**绝对路径**命名它的状态目录，把 `[A-Za-z0-9-]` 之外的字符全替换成 `-`：

```
D:\projects\sync-work    ->  D--projects-sync-work
/Users/me/projects/app   ->  -Users-me-projects-app
```

所以两台机器对这个目录名的看法必然不同。`sw ctx` 不在网络上传绝对路径 —— 每一侧从**自己的**检出路径推导**自己的**目录名，线上只传逻辑根名（`claude`）和相对路径。顺带一个好处：因为 `:`、`/`、`\` 都映射成 `-`，`git rev-parse` 在 Windows 上返回正斜杠这件事完全不影响结果。

### 合并规则

mtime 新的一方胜出。但在覆盖失败方之前，会**在本地**比较两者字节：如果失败方是胜出方的前缀，说明胜出方只是它的追加延续（append-only 会话记录的正常情况），覆盖是无损的，直接写。只有真正分叉的文件才会留下 `.conflict-<host>-<时间戳>` 副本。**任何情况下都不丢数据。**

写入时会把发送方的 mtime 一起带过去。少了这一步，刚写入的副本看起来更新，下次同步会把它推回去，来回弹跳永不收敛。

`quiet_secs`（默认 5 秒）内被改过的文件会跳过 —— 大概率正在被一个活跃会话追加写入。

**注意**：同一时刻只在一台机器上开同一个项目的会话。两边同时写必然分叉，工具会保住两份并告警，但那需要你手工处理。

## 工作原理

```
        Windows                                        Mac
┌────────────────────────┐                  ┌────────────────────────┐
│ sw verify (在 myapp 里) │                  │ sw node (每台机器一个)  │
│  1 扫描工作区           │                  │                        │
│    临时 index，不碰你的  │                  │                        │
│    暂存区               │                  │  2 查注册表：           │
│  2 diff → zstd  ────────┼──── iroh QUIC ──▶│    myapp -> ~/code/myapp│
│    + 项目名             │    公钥认证/加密   │  3 checkout base       │
│                        │                  │    git apply patch     │
│  6 日志/诊断/退出码 ◀───┼─── 流式事件 ──────┤  4 跑配方              │
└────────────────────────┘                  │  5 .sw/wt/<peer>/       │
                                            └────────────────────────┘
```

几个刻意的设计选择：

**快照用临时 index。** `sw verify` 绝不动你真实的暂存区 —— 在准备 commit 的过程中被工具改了 staging area 是不可接受的意外。

**worktree 复用而非重建。** 对端在 `<仓库>/.sw/wt/<peer>/` 保留一个持久 worktree，`target/` 缓存因此存活。这是远端验证能进内层开发循环而不是感觉像 CI 的原因。清理用 `git clean -fd`（**没有** `-x`）：上一次 patch 的残留文件被清掉，被 ignore 的构建产物保留。因为 patch 本身永不包含被 ignore 的文件，这个语义是自洽的。

**诊断结构化。** 配方里的 cargo 命令会被自动注入 `--message-format=json-diagnostic-rendered-ansi`（`cargo test --all -- --nocapture` 这类带 `--` 的会正确插在分隔符之前）。每行 stdout 先尝试按 cargo JSON 解析，成功就转成带 `file:line` 的结构化诊断，失败就当普通日志透传 —— 这样 `cargo test` 混合输出也能正确处理。

**授权就是对等表。** iroh 连接天生经过公钥认证（remote id 就是完成 QUIC 握手的那把公钥）。节点与节点之间，「id 在本机对等表里」就是充分的鉴权。没有 token 需要轮转或泄漏。未配对的连接直接关闭，不回复任何信息。

**机器密钥只归节点，客户端用一次性密钥 + 委托签名。** relay 对同一把 key 只保留一条活跃连接 —— 第二条注册会把第一条顶下线（iroh-relay 源码原话："multiple connections found, deactivating old connection"）。所以客户端若用机器密钥拨号，就把本机**节点**从 relay 上顶掉了：`sw ctx --watch` 挂着的那几个小时里，对端的 `sw doctor` 全部超时，而节点明明活着。实际发生过。

于是每个客户端进程生成一把一次性密钥来拨号，请求信封里带上机器密钥对这把一次性公钥的签名。对端验签后按**机器**授权 —— 效果与机器密钥亲自拨号完全相同，但 relay 上那把 key 永远只有节点一条连接。签名偷走也没用：QUIC 握手已经证明发送方持有一次性密钥的私半，签名离开那条连接毫无意义。唯一例外是配对（ticket 里带的就是机器公钥，必须由它上线发布），一次性命令，可接受。

**地址自己记，不靠公共目录。** 只有公钥的话，拨号就依赖某个公共服务愿意把 key 翻译成地址。这个假设不成立 —— 实测遇到过：n0 的权威 DNS 对一台机器正常返回，对同一对里的另一台返回 `Non-existent domain`，而那条记录在同一个服务自己的 pkarr 存储里明明存在且是新的。结果一半方向拨不通，两台机器都看不出原因。

所以每个请求都带上**发送方自己的** relay 地址，接收方记进对等表（`sw doctor` 会显示 `address`）。通过一次之后，这对机器就再不需要那个查询了。拨号时缓存地址和公共查询**同时**进行 —— iroh 把给定地址当作一条候选路径而非替代品 —— 所以地址过期只是浪费一次探测，不会让命令失败。

地址由**节点**记录（它启动时等到 online 才写），短命令从配置里读。因为一个 `sw ctx` 往往在自己的 endpoint 找到 relay 之前就跑完了 —— 实测 relay 上线约需 3 秒。

**路径不过网络。** 请求里只有项目名。对端在自己的注册表里查自己的路径，还会核对那个检出的 `syncwork.toml` 里的项目名是否匹配，防止注册表过期指向别的项目。peer 提供的相对路径在落盘前逐段校验，不能逃出根目录。

**base commit 缺失时自动回退。** 如果对端没有你的 HEAD（本地独有的 commit），它会先 `git fetch`；仍然没有就回报，客户端自动改用与远端的 merge-base 重新快照并加宽 patch。所以本地独有 commit 也不需要 push。

## 环境变量

| 变量 | 作用 |
|---|---|
| `SW_HOME` | 覆盖机器状态目录（默认 `~/.syncwork/`）。同一台机器上跑多个独立节点时用（端到端测试即靠它）。设了它时 `sw reset` 不会碰机器全局的开机自启项。 |
| `SW_LOG` | `tracing` 过滤器。默认 `error,syncwork=info` —— 节点的每请求日志加上任何来源的错误，但**不含依赖库的警告**（那些描述传输层内部状态，用户无从下手，还会插进正常输出中间）。排查连通性用 `SW_LOG=debug`。日志时间戳用**本机时区**并带偏移量（`2026-08-24 14:05:58.805+08:00`），因为日志是人对着钟看的，而且要和对端日志对时间。 |
| `SW_REMOTE` | 由 `sw` 注入到配方步骤中，值为 `1`。配方可用它区分远端执行。 |
| `NO_COLOR` | 关闭着色。非终端输出本来就不着色。 |
| `CLAUDE_CONFIG_DIR` | 覆盖 `~/.claude` 的位置，`sw ctx` 会遵循。 |

## 源码结构

```
src/
├── main.rs           入口：初始化着色、解析参数、分发
├── cli.rs            命令行定义（只有定义，没有实现）
├── commands/         本地事务性命令
│   ├── pair.rs       机器配对
│   ├── project.rs    init / link / unlink / projects
│   ├── hook.rs       Claude Code Stop hook 的装卸
│   ├── doctor.rs     一次查清所有可能出问题的地方
│   └── reset.rs      恢复出厂设置
├── config/           两层配置
│   ├── platform.rs   宿主平台（不是配置，是关于宿主的事实）
│   ├── repo.rs       syncwork.toml：项目名、配方
│   └── machine.rs    机器状态：身份、对等表、项目注册表
├── transport/        线路层
│   ├── message.rs    Request / Event / CtxFrame
│   ├── frame.rs      长度前缀分帧
│   ├── endpoint.rs   绑定、拨号、鉴权
│   └── ticket.rs     配对 ticket
├── node.rs           常驻接受循环；只做路由
├── verify/           远端构建验证
│   ├── client.rs     发起方
│   ├── server.rs     节点方
│   └── exec.rs       配方执行（只有 server 用它）
├── ctx/              AI 上下文同步
│   ├── roots.rs      根的声明求值（每台机器各自解析）
│   ├── scan.rs       扫描成 manifest、路径校验
│   ├── merge.rs      合并策略（前缀检测、冲突保留）
│   ├── client.rs     发起方
│   └── server.rs     节点方
├── git.rs            git 管道：快照、worktree、patch
├── service.rs        开机自启
└── style.rs          终端着色，非终端时关闭
```

一个刻意的安排：**`verify/` 和 `ctx/` 各自同时拥有自己的两端**。重构前 verify 的服务端在 `node.rs` 里，而 ctx 的服务端在 `ctxwire.rs` 里 —— 同一种东西放在两个地方。改动任一侧的交互几乎必然要同时动两端，把它们分开放是在邀请二者漂移。

`node.rs` 现在只是路由器，不含任何一种请求的实现。`exec` 放进 `verify/` 是因为只有它的服务端用得到。`config/platform.rs` 单独拆出来是因为平台不是配置 —— 它是关于宿主的事实，配方、服务安装和线路协议都要查它。

没有为了"设计模式"而加的 trait 抽象或间接层。这里的优雅是对称和内聚。

## 测试

```bash
cargo test          # 57 个单元测试
bash tests/e2e.sh   # 端到端：两台机器、两个项目、一个节点，27 项检查
```

端到端覆盖单元测试到不了的部分：机器级配对、项目注册表、QUIC 传输、patch 传递、worktree 隔离、诊断回传、退出码透传、上下文同步。其中两项专门锁住本工具的核心主张 —— **两个项目共用一个节点和一次配对**，以及**对端未 link 时给出可操作的错误**。

## 排查

先跑这个：

```bash
sw doctor
```

它把这套东西可能出问题的地方一次全查：本机身份和**实际读到的**状态目录、节点在不在跑、自启是否注册、每个已 link 项目的路径与项目名是否对得上、每个对端是否可达（含直连/relay 路径）、公钥是否互认、以及**两端构建是否一致**。

有失败项退出 1，只有警告退出 0 —— 「另一台机器睡着了」不是配置错误。

想从零重来：

```bash
sw reset          # 清掉本机身份、对等表、项目注册表，需要确认
sw reset --yes    # 跳过确认
```

它删的只有这台机器自己的东西 —— **不碰**任何仓库的 `syncwork.toml`、`.sw/` 构建缓存、以及 `~/.claude` 下的 AI 上下文。删除前会列出将要失去什么，并提醒你：本机换了新身份，所以**两边都要重新配对**。

```
this machine
  ✓ identity    58c1190746e246c3…
  ✓ state       C:\Users\sdkpo\.syncwork
  ✓ build       b9f24fc
  ✓ address     https://aps1-1.relay.n0.iroh.link./
  ✗ node        not running — start it with `sw service start`
  ✓ autostart   registered

peers
  ✓ niugms-macbook-pro-local  reachable
      *direct 10.209.21.106:56274 5ms, relay ... 172ms
  ✓ address     https://aps1-1.relay.n0.iroh.link./ (cached)
  ✓ keys        agree (the peer accepted this machine's key)
  ! build       peer 80998d1, here b9f24fc
      if you just updated, the peer's node is still on old code: `sw service restart` there
```

这里每一项都对应我们真实排查过的一次故障。下面几条是它给出结论后的解释：

**`the peer refused us as unpaired`** —— 对端连上了但不认识你的公钥。它记录的是一个过期的 key，通常因为本机身份被重新生成过。两边各跑 `sw pair list` 对比：你的 `id` 和对端 peers 表里记的应该一致。不一致就重新配对。

**`no address for peer ...`** —— 本机没有它的缓存地址，公共查询（n0 的 DNS）也没给出答案。**这不是配置错误，重新配对也没用。** 缓存地址是在对端**向本机发过请求**时记下的，所以在那台机器上跑一次 `sw doctor` 就够了，然后这个方向就通了。如果一直不通，那台机器的节点大概没在跑。

**`could not reach peer ...; is 'sw node' running there?`** —— 有地址但连不上：对端节点没在跑，或者网络不通。先确认对端有 `sw node`；确认了还不通就 `SW_LOG=debug sw status` 看握手细节。

**`address none cached`（对端条目下）** —— 目前只能靠公共查询找到那台机器。等它向本机发一次请求就会自动记上。两边都在跑节点、且都用过一次的话，这一项就会一直是绿的。

**两个 shell 报告不同的 `id`** —— `sw pair list` 会打印它实际使用的 state 目录，先对比这个。某些沙箱化的终端（如 Windows 应用容器）会重定向 `%APPDATA%`，导致同一台机器上不同 shell 读到不同的配置。**配对要在你日常使用的那个 shell 里做**，否则 ticket 里带的是另一份身份。

**节点在做什么** —— 节点每收到一个请求打一行日志，含对端名、连接路径（direct/relay + 地址 + RTT）和结果。`sw status` 也会显示当前连接走的是直连还是 relay。

## 已验证

在真实的 Windows 11 ↔ macOS 组合上跑通，不是模拟：

| 能力 | 结果 |
|---|---|
| Windows → Mac 构建验证 | 冷缓存 1m39s，热缓存 1.6s |
| Mac → Windows 构建验证 | 通过（双向对等成立） |
| **未提交、未 push** 的改动送达对端 | `E0308` 带 `src/style.rs:106:18` 回传，退出码 101 |
| 打洞直连 | `*direct 5ms`，relay `172ms` 作为兜底 |
| `sw ctx` 双向同步 | 两侧各自新建的会话互相拉取成功 |
| 另一台机器 `/resume` 接续会话 | 成功看到对端会话内容 |
| macOS 开机自启（launchd）+ 日志 | 通过 |
| Windows 开机自启（`Run` 键）+ 日志 | 通过 |

## 已知边界

- **`sw ctx` 要求同一个会话文件同一时刻只被一台机器追加。** 不同会话（各机器上新建的）互不干扰，它们是不同文件。危险的是在两台机器上 `/resume` **同一个**会话并各自往下写 —— 那必然分叉。工具会保住两份并告警（`.conflict-<host>-<戳>`，不丢数据），但需要你手工合并。
- **Windows 上没有 keep-alive。** `Run` 键只在登录时启动一次；节点若崩溃，要到下次登录才会回来（macOS 的 launchd 有 `KeepAlive`，Linux 的 systemd 有 `Restart=always`）。这是不需要管理员权限的代价。
- **只有关系的两端都活着，缓存地址才有意义。** 两台机器都换了网络、且公共查询仍然不给答案的话，双方缓存的 relay 都过期了，只能等 n0 的 DNS 恢复，或者重新配对一次。
- **拨号仍然依赖 relay 可达。** 缓存地址解决的是「查不到 key」，不是「连不上」。两台机器在同一个内网、但公网整体不通时，靠的是 iroh 的局域网发现，不是这个缓存。
- **沙箱化终端可能读到不同的配置。** 见排查一节 —— 配对要在你日常使用的那个 shell 里做。

## 路线

- `sw tui` — ratatui 面板，机器状态、项目列表与构建历史
- 超过两台机器时的扇出验证（`sw verify --all`）
- Windows 上的 keep-alive（不用管理员权限的前提下）
