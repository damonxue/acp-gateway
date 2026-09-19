# Agent Gateway

**让 Coding Agent 跑在你的电脑上，用手机随时接管。**

[English](README.md) · [架构](docs/architecture.md) · [远程协议](docs/remote-protocol.md) · [安全](docs/security.md) · [IDE 集成](docs/ide-integration.md) · [AHP / 微信通道](docs/ahp.md) · [桌面 App 与网页端方案](docs/ui-implementation-plan.md)

Agent Gateway 是运行在开发者本机的守护进程。它用 [Agent Client Protocol（ACP）][acp] 与 Codex、
Claude Code、OpenCode、Gemini CLI 等 Agent 通信，并通过需要鉴权的 WebSocket 把这些 Session 暴露给
手机、浏览器等远程客户端。合上笔记本、拿起手机，Session 仍在继续：相同的输出、相同的 Tool Call、
相同的待确认权限请求。

它**不是**新的 Agent Runtime，而是：ACP Proxy + Session Manager + Remote Transport。

```text
           手机 / 浏览器
                 │  wss + 一次性 ticket
                 ▼
        ┌────────────────────────────────────┐
        │            Agent Gateway              │
        │  ACP 代理 · Session · 事件日志          │
        │  设备配对 · Ticket · Tunnel             │
        └──────────────────┬─────────────────┘
                         │ ACP（stdio 上的 JSON-RPC 2.0）
                         ▼
            Codex · Claude Code · OpenCode · …
```

## 为什么需要它

Agent 一跑就是几分钟。你不想全程守在电脑前，也不希望人一走开 Agent 就断了。Gateway 保证三件事：

1. **Session 属于 Gateway，不属于手机。** 客户端断开不会影响 Agent 进程。
2. **每个事件都先落库再广播。** 重连时按 `after_seq` 回放：不丢、不重、不会重跑 prompt。
3. **危险操作仍然需要你点头。** 权限请求会推送给正在观察的客户端，在有人回答前 Agent 一直阻塞。

## 快速开始

```bash
cargo build --release

# 1. 生成初始配置 ~/.agent-gateway/config.toml
./target/release/agent-gateway config init

# 2. 改成你实际安装的 Agent，然后校验
$EDITOR ~/.agent-gateway/config.toml
./target/release/agent-gateway config check

# 3. 启动守护进程
./target/release/agent-gateway run
```

另开一个终端：

```bash
agent-gateway agents                                  # 可用的 Agent
SID=$(agent-gateway sessions new --agent codex)       # 在当前目录启动一个 Session
agent-gateway sessions prompt $SID "测试为什么失败？"
agent-gateway sessions watch $SID                     # 实时跟随事件流
```

配对手机：

```bash
agent-gateway pair          # 打印二维码和 6 位配对码
agent-gateway devices list
agent-gateway devices revoke device_…
```

### 配置

位于 `~/.agent-gateway/config.toml`，模板里每一项都有中英文注释。

```toml
[gateway]
bind = "127.0.0.1:48100"   # 仅监听回环地址；远程访问走 tunnel
data_dir = "~/.agent-gateway"
log_level = "info"

[security]
ticket_ttl = "60s"          # WSS ticket 一次性、短时效
pairing_ttl = "5m"
trust_loopback = true       # 本地调用可以不带凭证

[relay]
endpoint = "https://relay.example.com"

[tunnel]
mode = "token"              # 或 "config"
token = "…"                 # 永不写入日志
hostname = "gw.example.com"

[[agents]]
id = "codex"
name = "Codex"
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp@latest"]
```

Agent **必须显式配置**：Gateway 不会自动扫描本机可执行文件。

## 两套协议

| | 参与方 | 形式 |
|---|---|---|
| **ACP** | Gateway ↔ Agent | Agent stdio 上的 JSON-RPC 2.0，语义不修改 |
| **Remote 协议** | Gateway ↔ 手机/浏览器 | WebSocket 上的 JSON，见 [docs/remote-protocol.md](docs/remote-protocol.md) |

两者分离是有意为之：Gateway 升级 ACP SDK 时不需要发新的 App；Remote 协议可以携带 ACP 不关心的
概念（machine、ticket、回放游标）。Gateway 的扩展方法放在 `gateway/*`、`device/*` 等命名空间，
不改变标准 ACP 的任何语义。

### 本地 HTTP API

仅回环地址可访问，供 CLI、脚本和 IDE 插件使用：

| 方法 | 路径 | 作用 |
|---|---|---|
| `GET` | `/health` | 机器信息、Session 计数、tunnel/relay 状态 |
| `GET` | `/machines/current` | 当前机器 |
| `GET` | `/agents` | 已配置的 Agent |
| `GET`/`POST` | `/sessions` | 列表 / 创建 |
| `GET` | `/sessions/{id}` | Session 及待处理权限请求 |
| `POST` | `/sessions/{id}/prompt` | 发送 prompt |
| `POST` | `/sessions/{id}/cancel` | 取消当前回合 |
| `POST` | `/sessions/{id}/permission` | 回复权限请求 |
| `POST` | `/sessions/{id}/close` | 关闭 Session 并停掉 Agent |
| `GET` | `/sessions/{id}/events?after_seq=` | 拉取历史事件 |
| `WS` | `/sessions/{id}/stream?after_seq=` | 回放 + 实时流 |
| `GET` | `/ahp/status` | 本地 AHP 通道与二维码登录状态 |
| `POST` | `/ahp/bind` | 本地显式绑定已有 Session |
| `POST` | `/ahp/unbind` | 解除通道绑定 |

以下接口允许从 tunnel 访问，各自携带凭证：`GET /health`、`POST /pairing/consume`、
`POST /devices/{id}/ws-ticket`、`WS /remote?ticket=…`。

## 安全模型

* 机器的 Ed25519 私钥永远只存在本机（`identity/`，权限 `0600`）。
* 配对码：一次性、短时效，并带 nonce，设备必须原样回传。
* WSS ticket：一次性、短时效、同时绑定 device 和 machine。
* 撤销设备后，它手里已经拿到的 ticket 也立即失效。
* 带 tunnel 请求头的请求永远不算本地请求——尽管 `cloudflared` 也是从 127.0.0.1 连进来的。
* 密钥在类型层面就无法被打印：tunnel token 是 `Secret`，`Debug`/`Display` 都输出 `***`。
* Relay 拿不到 prompt、Agent 输出、terminal 输出或源码——push 只携带“状态”，不携带内容。

详情与威胁模型：[docs/security.md](docs/security.md)。

## 工程结构

```text
crates/
  gateway-core/     领域层：Session、事件、权限、端口抽象、SessionManager
  gateway-config/   TOML 配置、校验、密钥包装
  gateway-store/    SQLite 适配器
  gateway-auth/     机器身份、设备配对、WSS ticket
  gateway-acp/      ACP Agent 运行时、事件映射、客户端文件系统
  gateway-remote/   本地 HTTP API + 远程 WebSocket 协议
  gateway-tunnel/   cloudflared 监管器
  gateway-relay/    Relay 客户端、push worker、参考 Relay 服务端
  gateway-ahp/      出站 Agent Host Protocol / 微信通道
  gateway-cli/      `agent-gateway` 二进制（组装根）
migrations/         SQLite 表结构
```

`gateway-core` 不依赖任何传输层、数据库或 Agent 实现：所有外部依赖都是 `gateway_core::ports` 或
`gateway_core::agent` 里的 trait。正是这个倒置让手机无需关心 Session 是 Gateway 启动的还是（未来）
IDE 启动的。

## 开发

```bash
cargo test --workspace      # 单测 + 集成测试（包含真实 ACP Agent 进程）
cargo clippy --workspace --all-targets
cargo fmt --all
```

测试会启动 `fake-acp-agent`（`gateway-acp` 中一个真实的 ACP Agent），因此 prompt、流式输出、
Tool Call、权限请求和取消都是在真实的 JSON-RPC stdio 连接上验证的，而不是 mock 协议。

参见 [docs/development.md](docs/development.md)。

## 当前状态

已可用：Agent 启动、prompt/cancel、流式输出、Tool Call、权限请求、事件回放、断线重连、设备配对、
ticket、本地 API、远程 WebSocket 协议、cloudflared 监管、Relay 客户端与参考 Relay，以及支持显式
Session 绑定和二维码状态的出站 AHP 通道。详见 [AHP / 微信通道](docs/ahp.md)。

部分实现：让 Zed / VS Code 接入同一个 Session 的 IDE Bridge 现在已有
`agent-gateway acp-bridge` 入口和 daemon `/bridge` socket，但多 Session 编排和更细的仲裁策略
仍在后续工作里。

尚未实现：客户端 terminal 能力、参考 Relay 的真实 APNs/FCM 推送。

[acp]: https://agentclientprotocol.com/
