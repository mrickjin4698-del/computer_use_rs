# Alice Computer Use

[English](README.md) | [简体中文](README.zh-CN.md)

一个使用 Rust 编写、面向 Windows 和 macOS 的安全优先、模型无关的计算机操作运行时。

本项目可以捕获桌面画面、在平台后端支持时读取原生 UI 元素、执行语义操作或像素操作，
并在智能体与原生输入注入之间提供策略和审批 Broker。它既可以通过 Rust API 嵌入应用，
也可以作为长期运行的 Sidecar 或本地 MCP Server 使用。

> 当前状态：实验性。1.0 版本之前，协议和公开 Rust API 可能发生变化。

## 设计目标

- 将原生桌面访问能力隔离在模型宿主进程之外。
- 优先执行 UI Automation 语义操作，而不是坐标点击。
- 将元素引用绑定到观测代次，拒绝过期操作。
- 将密码输入框、高权限窗口、安全桌面和用户操作干扰视为明确的安全边界。
- 输入分发前必须经过宿主拥有的租约和审批策略。
- 截图和审计证据必须有容量限制，并且默认不保留。

## Workspace 结构

| Crate | 用途 |
| --- | --- |
| `alice-computer-use-core` | 共享领域类型、观测结果、操作、能力声明和错误类型 |
| `alice-computer-use-runtime` | 原生显示器截图、窗口检查和输入执行 |
| `alice-computer-use-sidecar-protocol` | Sidecar 使用的长度分帧 JSON RPC 协议 |
| `alice-computer-use-sidecar-client` | 宿主侧进程和会话客户端 |
| `alice-computer-use-broker` | 租约、策略、审批、干扰检测、审计和幂等分发 |
| `alice-computer` | 长期运行的原生 Sidecar 可执行程序 |
| `alice-computer-mcp` | 对外提供 Broker 工具的本地 stdio MCP Server |

本仓库有意排除了原应用的 Tauri 适配层以及特定智能体的提示词和工具集成。
接入方需要自行实现审批界面、生命周期管理和策略配置。

## 平台支持

原生运行时支持 Windows 10/11 和 macOS。macOS 支持显示器枚举、逐显示器和跨显示器
虚拟桌面 PNG 截图、窗口元数据、后台优先的 AX/PID 输入、Accessibility 窗口激活，
以及 CoreGraphics 输入活动监控。macOS 上的 AXUIElement 现已支持有界语义观测，以及
聚焦、调用、值设置、切换、选择、展开/折叠和范围值操作；没有可移植实现的滚动到可见
位置操作仍会明确报告为能力缺口。

在 macOS 上，发布版可以额外打包 Swift 原生 helper。它使用稳定的
`com.alice.computer.native` App 身份和 designated requirement 管理 TCC，并使用
ScreenCaptureKit 截图；Rust 仍负责 Broker、安全策略、坐标契约、帧缓存和
CoreGraphics 输入：

```text
cargo build --release -p alice-computer -p alice-computer-mcp
sh scripts/build-macos-native-app.sh release
```

helper 会自动从发布版程序旁边发现。第一次截图未获授权时，它会请求“屏幕录制”，
如果 macOS 没有弹出标准对话框则打开对应的系统设置页。请授权名为
`Alice Computer Native` 的条目。如果之前已经添加过旧的 ad-hoc
`AliceComputerNative`，切换到稳定 requirement 后需要先移除再重新添加一次；没有
app 包的部署会继续使用 CoreGraphics 回退路径。

macOS 需要为 Sidecar 授予“屏幕录制”权限才能截图，为“辅助功能”权限才能注入
输入，并为“输入监控”权限启用 Broker 的用户活动监控。输入活动监控不可用时，
Broker 会拒绝有副作用的操作，但仍允许观测。

## 构建与测试

安装当前稳定版 Rust 工具链，然后运行：

```text
cargo build --workspace
cargo test --workspace
```

仓库中的交互式验收 fixture 属于 examples，不会被常规测试命令构建；请在对应平台上
使用 `cargo run --features interactive-e2e --example ...` 手动运行具体 fixture。

构建发布版 Sidecar 和 MCP Server：

```text
cargo build --release -p alice-computer -p alice-computer-mcp
```

生成的程序位于 `target/release/`。

## MCP Server

先构建两个可执行程序，然后运行：

```text
target/release/alice-computer-mcp --sidecar target/release/alice-computer
```

MCP Server 通过 stdio 通信。标准输出必须仅用于协议帧，诊断信息会写入标准错误。

MCP Adapter 和 Sidecar 会惰性启动原生桌面运行时。显示器暂不可用或 macOS 权限暂时
不可用时，健康检查会返回结构化错误，唤醒/授权后可在不重启 MCP stdio 进程的情况下重试。
只包含 `wait` 的 `computer_use` 批次不会启动桌面 session，也不会枚举窗口。

Server 提供桌面观测和操作工具。需要兼容 Codex/Computer Use 动作循环时，使用
`computer_use`：传入一个 `actions` 数组，动作类型包括 `click`、`double_click`、
`scroll`、`type`、`wait`、`keypress`、`drag`、`move` 和 `screenshot`。坐标是当前
截图中的像素坐标。指针动作会绑定到一次当前帧；默认只返回紧凑的动作结果，不会额外
编码和传输 PNG；指针动作和显式 `screenshot` 动作仍会返回图像。需要强制返回或禁用
图像时分别设置 `include_screenshot=true` 或 `false`。

`computer_use` 的一次调用最多执行 64 个动作，并在 stdio MCP 场景使用请求级
Autonomous broker profile，以免等待无法由 MCP stdio 完成的审批 UI。Broker 仍会执行
前台窗口、帧新鲜度、目标安全边界、桌面租约和用户干扰检测。请只把 MCP Server 接入
你信任的本地 Agent；涉及登录、支付、删除等高影响操作时，仍应在 Agent 层保留人工确认。
`computer_execute` 在本地 MCP 默认同样使用 Autonomous profile，因为 stdio 没有审批恢复
回调；设置 `ALICE_COMPUTER_MCP_REQUIRE_APPROVAL=1` 可恢复 Conservative 审批模式。无论
哪种模式，能力、前台窗口、租约、输入监控和目标安全边界仍由 Broker 强制执行。

批处理内部区分两种目标：像素/鼠标操作绑定到当前截图对应的精确窗口；普通键盘和文本输入
绑定到已观测应用的进程，而不是固定的旧顶层窗口。macOS 上 `execution_mode` 默认是
`background_preferred`：AX 操作和 PID 定向输入不会激活目标，也不会移动用户的真实光标；返回的
截图会绘制 session 级虚拟光标轨迹。没有可靠后台路径的控件会自动 fallback 到前台接管式操作；
需要兼容旧行为时可设置 `takeover_only`。`⌘Tab`、`⌘Space` 等可能改变桌面焦点的快捷键仍走
全局输入路径，完成后会重新枚举前台应用、能力和截图帧，再执行后续动作。这使浏览器开新标签
或切换应用时不会把输入继续投递到旧窗口，同时保留坐标点击的严格安全校验。

在 macOS 上，`DisplayPhysical` 缩放使用当前显示模式的真实像素尺寸；每个截图帧另外报告
其实际图像 backing scale。这样 Retina 或降采样显示器在截图像素与 Quartz 桌面坐标之间转换时
保持一致。

## 安全模型

计算机操作软件能够控制当前登录用户的桌面。不要将 Sidecar 或 MCP 进程暴露给不可信
网络，也不要在生产环境中绕过 Broker。

项目实现的主要安全保护包括：

- 拒绝过期画面帧和过期 UI 元素；
- 识别受保护窗口和高权限目标；
- 隐去密码控件内容并禁止修改其值；
- 验证前台窗口和观测代次；
- 使用短时桌面租约；
- 检测用户输入和外部操作干扰；
- 根据策略决定是否需要审批；
- 仅记录有容量限制的审计元数据，不记录原始 Windows 安全令牌。

集成本项目之前，请阅读 [SECURITY.md](SECURITY.md)。

## 仓库来源

本仓库从 Project Alice 独立实现的计算机操作子系统中提取。仓库不包含 OpenAI、
Anthropic、浏览器驱动或 Tauri 的源代码。第三方 Rust 依赖仍遵循各自的许可证。

## 许可证

本项目使用 Apache License 2.0，详情请参阅 [LICENSE](LICENSE)。
