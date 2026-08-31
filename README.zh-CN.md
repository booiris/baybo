<p align="center">
  <img src="assets/baybo.svg" width="120" alt="Baybo logo">
</p>

<h1 align="center">Baybo</h1>

<p align="center">
  <a href="README.md">English</a> | 简体中文
</p>

<p align="center">
  自托管、常驻运行的 AI 助手框架 ——<br>
  多渠道接入、工具调用、技能扩展,内建完整的上下文管理、成本核算与错误恢复。
</p>

---

Baybo 以单个守护进程(**gateway**)运行在你自己的机器上。通过内嵌 Web 面板、终端
UI、Telegram、微信或已配对的 iOS 应用与它对话——所有入口共享同一批会话,数据存在
本地 SQLite。背后的 agent 拥有完整的工具集(文件、Shell、Web、MCP),
能派生类型化子代理、跑定时任务、维护按 agent 隔离的长期记忆,并把每个回合记录成
可浏览的 trace,附带逐次调用的成本核算。

## 功能

- **多渠道** —— Web 面板、终端 UI、一次性 CLI、Telegram 与微信机器人,以及端到端
  加密的 iOS 客户端。新渠道通过 TypeScript sidecar SDK 接入。
- **19 家 LLM 提供商** —— Anthropic、OpenAI(API key,或用 ChatGPT/Codex 订阅走
  OAuth —— 直连 Codex Responses API,无需 API key)、Gemini、DeepSeek、xAI、
  Mistral、Groq、Ollama、llamafile 等;按会话切换模型、配置热重载。
- **工具与 MCP** —— 内置 `Read`/`Write`/`Edit`/`Bash`/`Grep`/`WebFetch`/`WebSearch`
  等工具,还可通过 `baybo mcp add` 接入任意 MCP 服务器。
- **可扩展的 agent** —— 带信任分级和 LLM 风险评估的声明式技能、类型化子代理配置、
  可委托给外部 `claude` / `codex` CLI,以及一组 agent 在 git worktree 中协作处理
  issue 的看板。
- **常驻运行** —— 对话式创建 cron 定时任务、后台任务、内置文件式记忆(含周期性
  "做梦"整理),可选接入外部记忆后端(mem0、OpenViking)。
- **默认安全** —— 加密密钥保险库、Shell 命令 OS 沙箱、审批门、入站消息按用户配对、
  密钥泄漏检测。
- **可观测** —— 每个工作单元都是一个 Turn,带完整 trace 树和 token/成本核算,可在
  Web 面板中浏览。

## iOS 应用

原生 iOS 客户端([`app/ios`](app/ios)):扫码与 gateway 配对,之后通过端到端加密
传输聊天 —— 推送通知预览同样加密,中继和 Apple 只见密文。

```bash
baybo device pair    # 打印二维码;App 内扫码,两端确认配对码
```

配对——以及手机无法直连 gateway 时的聊天——都经由**盲中继**转发,因此 NAT 后面的
gateway 不需要公网地址。默认使用托管中继 `wss://proxy.baybo.space`(推送:
`https://push.baybo.space`)及内置试用 key;**该中继仅供试用,不保证稳定性**。
中国大陆请使用 CN 节点:

```bash
baybo device pair --proxy-url cn-proxy.baybo.space
```

生产使用请自建中继([`remote-host/DEPLOY.md`](remote-host/DEPLOY.md)),并传入自己
的 `--proxy-url` / `--push-url` / `--remote-api-key`。App 从 [`app/ios`](app/ios)
构建安装;详见
[`docs/modules/mobile/companion.md`](docs/modules/mobile/companion.md)。

## 环境要求

Baybo 只支持 **Linux 和 macOS**。

| 依赖 | 用途 |
|---|---|
| Rust 工具链(rustup) | 构建 —— 版本由 `rust-toolchain.toml` 锁定 |
| `pnpm` | 构建 —— Web 面板和 sidecar 会编译进二进制 |
| `git` | 运行时(工作区身份仓库) |
| `rg`(ripgrep) | agent 的 `Grep`/`Glob` 工具 |
| `bun` | Telegram/微信 sidecar 与 Deck 卡片(构建 + 运行时) |
| `node` | 仅浏览器工具 sidecar |
| `bwrap` / `sandbox-exec` / `docker` | Shell 命令 OS 沙箱(推荐) |

## 构建

```bash
git clone https://github.com/booiris/baybo && cd baybo
pnpm install
cargo build --release       # 产出 target/release/baybo
```

缺少 bun 时 sidecar 包会被嵌入为空(Telegram/微信无法启动)——
`BAYBO_REQUIRE_SIDECARS=1` 可让这种情况直接构建失败。缺少 pnpm 时面板构建会硬失败;
`BAYBO_SKIP_WEBUI=1` 跳过面板。把二进制放进 `PATH`:
`cargo install --path crates/baybo`。

## 快速开始

```bash
baybo setup            # 首跑向导:工作区、加密密钥、LLM 提供商
baybo gateway start    # 启动守护进程(打印面板 URL + 管理 token)
```

然后在第二个终端开始对话:

```bash
baybo tui                                   # 终端聊天
baybo prompt "介绍一下你自己"                # 一次性回答,输出到 stdout
git diff | baybo prompt "review this"       # 管道 stdin 作为上下文
```

或打开 **Web 面板** `http://127.0.0.1:8888` 并粘贴 token(`baybo gateway token show`
可找回)。装成常驻服务:

```bash
baybo gateway install    # systemd 用户单元(Linux)/ launchd agent(macOS)
baybo gateway enable     # 铸造管理 token,开机自启并立即启动
```

> debug 构建的工作区根目录是 `./.baybo`,release 构建是 `~/.baybo`。

## 日常命令

```text
baybo status [--live]     健康/清单快照
baybo doctor              就绪检查:配置、存储、LLM 探活
baybo llm …               LLM 提供商条目(status / probe / add / edit / default)
baybo channel …           聊天机器人(list / add / remove)
baybo mcp …               MCP 服务器(add / list / get / remove)
baybo secret …            可注入 Bash 的保险库密钥
baybo pair …              审批/吊销入站渠道用户
baybo gateway …           守护进程生命周期 + 管理 token
baybo skills …            查看技能
baybo memory …            外部记忆后端
baybo external-agent …    claude / codex 委托
baybo completion <shell>  shell 补全
```

另有一组运维命令族(`config`、`session`、`turn`、`cron`、`log`、`cost`)默认不在
`--help` 中显示,设 `BAYBO_HELP_AGENT=1` 可列出。多数只读命令在任意聊天里也能以
斜杠命令使用(`/status`、`/config show` 等);变更型需要 `--yes`。日志:
`RUST_LOG=baybo=debug`,文件在 `<workspace>/logs/`,`baybo log main -f` 实时跟踪。

## 配置

一个 JSON 文件:`~/.baybo/config/baybo.json`(可用 `--config` 或
`BAYBO_CONFIG_PATH` 覆盖);[`baybo.example.json`](baybo.example.json) 是起点。

| 配置段 | 控制内容 |
|---|---|
| `llm`、`default-llm` | 命名的提供商条目与默认项 —— 注意 `default-llm` 带**横杠** |
| `agent` | 迭代上限、上下文预算/压缩、子代理深度、模型分层 |
| `channels` | 终端渠道开关(机器人通过 `baybo channel add` 注册,不走配置) |
| `permission` | Bash 审批策略:`auto`(默认)、`manual`、`free` |
| `gateway` | 绑定地址/端口(默认 `127.0.0.1:8888`) |
| `cost` | 花费上限与速率限制 |
| `memory` | 内置文件记忆 + dream 调度;外部后端 |
| `web_search`、`browser`、`proxy`、`external_agents`、`security`、`skills`、`workspace` | 见 [`docs/modules/config.md`](docs/modules/config.md) |

API key 从不落在配置文件里 —— 解析顺序:`api_key_env` → 加密保险库(`baybo llm
add` 写入的位置)→ 提供商惯例环境变量。修改配置用 `baybo config set <path>
<value>` 后重启;`llm`、`cost` 上限、`web_search`、`permission` 可经 SIGHUP 热重载
([`docs/config-hot-reload.md`](docs/config-hot-reload.md))。

## 聊天渠道

Telegram 和微信树内自带:

```bash
baybo channel add        # 选择渠道,粘贴 bot token(微信为扫码)
```

运行中的 gateway 几秒内拉起新机器人。陌生发信人须先配对才能对话:

```bash
baybo pair approve <CODE>
```

开发自己的渠道:实现 [`sidecars/sdk/channel-ts`](sidecars/sdk/channel-ts) 中的
`Channel` 接口,把包放到 `sidecars/channel/<name>/` —— 见
[`docs/sidecars.md`](docs/sidecars.md)。

## Docker 部署

```bash
cd deploy/docker
cp .env.example .env     # 设置 BAYBO_LLM_API_KEY
docker compose up --detach --build
docker compose exec baybo baybo gateway token show   # 面板登录 token
```

打开 `http://localhost:8888`。会话数据在 `baybo-data` 卷里 —— 绝不要
`docker compose down --volumes`。详见
[`deploy/docker/README.md`](deploy/docker/README.md)。

手机在 NAT 后面时,[`remote-host/`](remote-host) 是独立部署的盲中继 + APNs 推送
([`remote-host/DEPLOY.md`](remote-host/DEPLOY.md))。

## 扩展 Baybo

- **技能(Skills)** —— 每个技能一个目录:`personas/<agent>/skills/<name>/SKILL.md`;
  用 `/<name>` 调用或由模型自动选择。
  [`docs/modules/skills.md`](docs/modules/skills.md)
- **子代理(Subagents)** —— 每种类型一个 Markdown 配置:`<workspace>/agents/<name>.md`;
  经 `spawn_subagent` 派生。[`docs/modules/subagent.md`](docs/modules/subagent.md)
- **外部 agent** —— 子代理可跑在宿主机的 Claude Code 或 Codex CLI 上,不受 Baybo
  沙箱约束。[`docs/external-agents.md`](docs/external-agents.md)
- **MCP 服务器** —— `baybo mcp add` 写入 `<workspace>/config/.mcp.json`;工具以
  `<server>/<tool>` 形式暴露。[`docs/modules/tools.md`](docs/modules/tools.md)
- **Cron 定时任务** —— 对话式创建;每次触发都是带 trace 的真实会话。
  [`docs/modules/cron.md`](docs/modules/cron.md)
- **记忆** —— 按 agent 的 markdown 记忆 + dream 任务,默认开启;mem0 或 OpenViking
  经 `baybo memory setup` 接入。
  [`docs/modules/memory-builtin.md`](docs/modules/memory-builtin.md)
- **看板项目** —— agent 团队在按 issue 隔离的 git worktree 中协作。
  [`docs/modules/project.md`](docs/modules/project.md)
- **Deck 卡片** —— agent 编写的实时卡片,在 iOS 上渲染。
  [`docs/modules/deck.md`](docs/modules/deck.md)

## 截图

**Web 面板** —— 聊天、trace 查看器、用量分析:

<p align="center">
  <img src="assets/screenshots/web-chat.png" alt="Web 面板 — 聊天" width="100%">
</p>
<p align="center">
  <img src="assets/screenshots/web-trace.png" alt="Trace 查看器 — 一个回合的 span、token 与 I/O" width="49.2%">
  <img src="assets/screenshots/web-analytics.png" alt="Analytics — token 用量与成本" width="49.2%">
</p>

**iOS 应用** —— 会话列表、富文本回复与实时 HTML、看板、Deck:

<p align="center">
  <img src="assets/screenshots/ios-chats.png" alt="iOS — 会话列表" width="24%">
  <img src="assets/screenshots/ios-chat-html.png" alt="iOS — LaTeX 与实时 HTML 面板" width="24%">
  <img src="assets/screenshots/ios-board.png" alt="iOS — 看板" width="24%">
  <img src="assets/screenshots/ios-deck.png" alt="iOS — Deck 实时卡片" width="24%">
</p>

## 开发

```bash
cargo fmt
cargo clippy --all --benches --tests --examples --all-features   # 零警告
cargo nextest run --workspace
pnpm --filter @baybo/channel-sdk test
```

先读 [`docs/architecture.md`](docs/architecture.md),再看
[`docs/modules/README.md`](docs/modules/README.md) 索引的各模块设计文档。贡献者
规范:[`CLAUDE.md`](CLAUDE.md) · 项目方向:[`docs/roadmap.md`](docs/roadmap.md)。

## 许可证

[MIT](LICENSE)
