# Agent Gateway

**让 Coding Agent 跑在你的电脑上，用 AHP 协调多个客户端。**

[English](README.md) · [架构](docs/architecture.md) · [远程协议](docs/remote-protocol.md) · [安全](docs/security.md) · [IDE 集成](docs/ide-integration.md) · [聊天适配器](docs/chat-adapters.md)

Agent Gateway 是运行在开发者本机的守护进程。它用 [Agent Client Protocol（ACP）][acp] 与 Codex、
Claude Code、OpenCode、Gemini CLI 等 Agent 通信，并负责持久化 Session。多客户端的主要控制路径是
Agent Host Protocol（AHP）：多个 AHP client 可以同时连接 Gateway，订阅同一个 Session，接收同一份
事件流，并安全地竞争处理权限请求。

手机/浏览器的 `/remote` 是另一条较早的传输路径，依赖 relay 和 tunnel。目前 relay、tunnel 以及这条
远程路径都不稳定，应当按实验功能使用。

它**不是**新的 Agent Runtime，而是：ACP Proxy + AHP Host + Session Manager，以及可选的 Remote Transport。

```text
        多个 AHP client
      Zed · VS Code · AHPX · 各类 adapter
                 │  AHP WebSocket
                 ▼
        ┌────────────────────────────────────┐
        │            Agent Gateway              │
        │  AHP Host · ACP proxy · Session       │
        │  事件日志 · channel/session 绑定       │
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
agent-gateway zed config     # 输出 Zed 的 agent_servers JSON
agent-gateway zed copy       # 复制到剪贴板
```

### 手机端（实验功能）

Gateway 的 `/app` 是状态栏应用。它通过 `/remote` WebSocket 和 relay/tunnel 连接，当前稳定性不足，
可能出现断线或 Session 暂时不可用。需要多个客户端同时操作时，建议使用连接 Gateway `/ahp` 的 AHP client。

### 配置

位于 `~/.agent-gateway/config.toml`，模板里每一项都有中英文注释。

```toml
[gateway]
bind = "127.0.0.1:48100"   # 仅监听回环地址；/remote 使用实验性的 tunnel
data_dir = "~/.agent-gateway"
log_level = "info"

[security]
ticket_ttl = "60s"          # WSS ticket 一次性、短时效
pairing_ttl = "5m"
trust_loopback = true       # 本地调用可以不带凭证

# [relay]
# endpoint = "https://relay.example.com"

# [tunnel]
# mode = "token"              # 或 "config"
# token = "…"                 # 永不写入日志
# hostname = "gw.example.com"

# 可选的外部 AHP adapter；Gateway 内置的 /ahp Host 不需要这一段
[ahp]
endpoint = "wss://ahp.example.com/channel"
reconnect_interval = "3s"

[[agents]]
id = "codex"
name = "Codex"
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp@latest"]
```

Agent **必须显式配置**：Gateway 不会自动扫描本机可执行文件。

`app/` 提供原生桌面控制台：macOS 使用 AppKit 的 `objc2`、`objc2-foundation`、
`objc2-app-kit` 实现状态栏应用，Windows 使用微软官方 `windows` crate 实现 Win32 窗口。
点击 Refresh 会重新请求 `/sessions` 并重建列表；macOS 还会展示通道状态、微信绑定和二维码。

“Phone/browser pairing QR” 是给 Agent Gateway 手机/Web 客户端使用的 JSON 配对载荷，
不是微信登录二维码。用微信扫描它看到 JSON 是正常现象；微信登录二维码来自 AHP 通道，
两者在菜单中已分开显示。

Windows 发布包可在 PowerShell 中运行 `script/package-windows.ps1` 生成，包内包含 daemon、桌面
App 和 README。提交到 `main` 后，`.github/workflows/release.yml` 会自动构建 Windows ZIP 和 macOS DMG，
并发布为 GitHub pre-release。

App bundle 内有两个 CLI：`agent-gateway` 是 Zed 调用的 bundle wrapper，
`agent-gateway-daemon` 是状态栏启动的 web-enabled daemon；二者都直接链接同一个
`gateway-cli` Rust library。

每个 `[[agents]]` 都可以生成如下 Zed 配置：

```json
{
  "agent_servers": {
    "Codex via Agent Gateway": {
      "default_config_options": { "model": "gpt-6-astra" },
      "type": "custom",
      "command": "/path/to/agent-gateway",
      "args": ["acp-bridge", "--agent", "codex"],
      "env": { "AGENT_GATEWAY_CONFIG": "/Users/me/.agent-gateway/config.toml" }
    }
  }
}
```

也可以直接执行 `agent-gateway zed copy`，或将 `agent-gateway zed config` 的结果粘贴到
Zed `settings.json` 的 `agent_servers` 下。

CLI 路径按以下顺序自动发现：`AGENT_GATEWAY_BIN`、App bundle 中的同目录 CLI、选择的 Cargo
项目（`AGENT_GATEWAY_PROJECT`）、workspace 的 `target/debug` 或 `target/release`，最后是 `PATH`。
安装后的 App 会优先使用同目录的 `agent-gateway` wrapper，因此 Zed 不依赖源码 checkout。

## 三条协议边界

| | 参与方 | 形式 |
|---|---|---|
| **ACP** | Gateway ↔ Agent | Agent stdio 上的 JSON-RPC 2.0，语义不修改 |
| **AHP** | 多个 client ↔ Gateway | AHP WebSocket；多个 client 共享 Session、事件流和权限状态 |
| **Remote 协议** | Gateway ↔ 手机/浏览器 | WebSocket 上的 JSON，见 [docs/remote-protocol.md](docs/remote-protocol.md) |

ACP 和 AHP 分离是有意为之：Gateway 升级 ACP SDK 时不需要改变多客户端控制面；AHP 负责 client、
Session、事件和权限的共享。Remote 协议保留给手机/浏览器的 relay/tunnel 路径，目前不稳定。Gateway
的扩展方法放在 `gateway/*`、`device/*` 等命名空间，不改变标准 ACP 的任何语义。

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
| `WS` | `/ahp` | 面向多个本地 client 的 AHP Host |

以下接口属于实验性的 relay/tunnel 远程路径，各自携带凭证：`GET /health`、`POST /pairing/consume`、
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
  gateway-wechat/   内嵌微信 Bot 适配器
  gateway-lark/     Lark/飞书签名回调与消息 API 适配器
  gateway-telegram/ Telegram Bot API 长轮询适配器
  gateway-cli/      `agent-gateway` 二进制（组装根）
app/                macOS/Windows 原生桌面 companion（独立 cargo workspace）
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

当前稳定性：

| 路径 | 状态 | 范围 |
|---|---|---|
| 本地 ACP runtime 与 SessionManager | 稳定 | Agent 进程、持久化 Session、事件和权限 |
| AHP Host（`/ahp`） | 主要多客户端路径 | 多个 client 订阅并操作 Gateway Session |
| 微信 adapter | 稳定 | 当前稳定的聊天通道，支持显式 Session 绑定 |
| Relay + tunnel + `/remote` | 实验性 / 不稳定 | 手机/浏览器传输，断线和可用性不保证 |
| Lark / 飞书 adapter | 实验性 / 不稳定 | 已提供，但不承诺稳定性 |
| Telegram adapter | 实验性 / 不稳定 | 已提供，但不承诺稳定性 |

AHP 协议本身不提供微信登录；微信 adapter 负责登录和二维码，再把消息映射到选定的 Gateway Session。
详见 [AHP / 微信通道](docs/ahp.md) 和 [聊天适配器](docs/chat-adapters.md)。

部分实现：让 Zed / VS Code 接入同一个 Session 的 IDE Bridge 现在已有
`agent-gateway acp-bridge` 入口和 daemon `/bridge` socket，但多 Session 编排和更细的仲裁策略
仍在后续工作里。

尚未实现：客户端 terminal 能力、参考 Relay 的真实 APNs/FCM 推送。

[acp]: https://agentclientprotocol.com/
