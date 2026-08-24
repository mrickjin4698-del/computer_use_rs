# Alice Computer Use

[English](README.md) | [简体中文](README.zh-CN.md)

一个使用 Rust 编写、面向 Windows 的安全优先、模型无关的计算机操作运行时。

本项目可以捕获桌面画面、读取 Windows UI Automation 元素、执行语义操作或像素操作，
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
| `alice-computer-use-runtime` | Windows 截图、UI Automation、窗口检查和输入执行 |
| `alice-computer-use-sidecar-protocol` | Sidecar 使用的长度分帧 JSON RPC 协议 |
| `alice-computer-use-sidecar-client` | 宿主侧进程和会话客户端 |
| `alice-computer-use-broker` | 租约、策略、审批、干扰检测、审计和幂等分发 |
| `alice-computer` | 长期运行的原生 Sidecar 可执行程序 |
| `alice-computer-mcp` | 对外提供 Broker 工具的本地 stdio MCP Server |

本仓库有意排除了原应用的 Tauri 适配层以及特定智能体的提示词和工具集成。
接入方需要自行实现审批界面、生命周期管理和策略配置。

## 平台支持

主要支持 Windows 10 和 Windows 11。协议与策略 crate 可以在其他平台编译，
但桌面观测和操作执行依赖 Windows API。

## 构建与测试

安装当前稳定版 Rust 工具链，然后运行：

```powershell
cargo build --workspace
cargo test --workspace
```

构建发布版 Sidecar 和 MCP Server：

```powershell
cargo build --release -p alice-computer -p alice-computer-mcp
```

生成的程序位于 `target/release/`。

## MCP Server

先构建两个可执行程序，然后运行：

```powershell
target/release/alice-computer-mcp.exe --sidecar target/release/alice-computer.exe
```

MCP Server 通过 stdio 通信。标准输出必须仅用于协议帧，诊断信息会写入标准错误。

Server 提供桌面观测和操作工具。客户端应当始终先观测再操作，只使用最新的画面帧和
元素引用。操作结果不确定时，应重新观测，不要盲目重复操作。

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
