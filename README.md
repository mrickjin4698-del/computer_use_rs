# Alice Computer Use

[English](README.md) | [简体中文](README.zh-CN.md)

A security-oriented, model-agnostic computer-use runtime for Windows and macOS, written in Rust.

The project captures desktop observations, exposes native UI elements where the
platform backend supports them, executes semantic or pixel actions, and places a
policy/approval broker between an agent and native input injection. It can be
embedded through Rust APIs, run as a long-lived sidecar, or exposed as a local MCP
server.

> Status: experimental. The protocol and public Rust APIs may change before 1.0.

## Design goals

- Keep native desktop access outside the model-facing host process.
- Prefer semantic UI Automation actions over coordinate clicks.
- Bind element references to an observation generation to reject stale actions.
- Treat password fields, elevated windows, secure desktops, and user interference
  as explicit security boundaries.
- Require host-owned leases and approval policy before dispatching input.
- Keep screenshots and audit evidence bounded and opt-in.

## Workspace

| Crate | Purpose |
| --- | --- |
| `alice-computer-use-core` | Shared domain types, observations, actions, capabilities, and errors |
| `alice-computer-use-runtime` | Native display capture, window inspection, and input execution |
| `alice-computer-use-sidecar-protocol` | Length-delimited JSON RPC protocol used by the sidecar |
| `alice-computer-use-sidecar-client` | Host-side process and session client |
| `alice-computer-use-broker` | Leases, policy, approvals, interference detection, audit, and exactly-once dispatch |
| `alice-computer` | Long-lived native sidecar executable |
| `alice-computer-mcp` | Local stdio MCP server exposing the brokered tools |

The repository deliberately excludes the original application's Tauri adapter and
agent-specific prompt/tool integration. Applications are expected to own their
approval UI, lifecycle, and policy configuration.

## Platform support

Windows 10/11 and macOS are supported by the native runtime. The macOS runtime
provides display enumeration, per-display and cross-display virtual desktop PNG
capture, window metadata, background-preferred AX/PID input, Accessibility
window activation, and a CoreGraphics input activity monitor. The AXUIElement backend provides bounded semantic observation and
focus, invoke, value, toggle, selection, expansion, and range actions; the
portable scroll-into-view action remains an explicit capability gap.

On macOS, release builds can include the Swift native helper. It uses a stable
`com.alice.computer.native` app identity and designated requirement for TCC,
and ScreenCaptureKit for display capture, while Rust retains the broker,
coordinate contract, frame store, and CoreGraphics input path:

```text
cargo build --release -p alice-computer -p alice-computer-mcp
sh scripts/build-macos-native-app.sh release
```

The helper is discovered automatically beside the release binaries. On its
first capture it requests Screen Recording and opens the matching System
Settings pane if macOS does not show a prompt. Grant the entry named `Alice
Computer Native`. If an older ad-hoc `AliceComputerNative` entry already
exists, remove and re-add it once after switching to this stable requirement;
deployments without the app bundle continue to use the CoreGraphics fallback.

macOS requires the sidecar to be granted Screen Recording permission for display
capture, Accessibility permission for input injection, and Input Monitoring
permission for the Broker's user-activity monitor. If the activity monitor is
unavailable, Brokered side-effect actions fail closed while observation remains
available.

## Build and test

Install a current stable Rust toolchain, then run:

```text
cargo build --workspace
cargo test --workspace
```

The repository's interactive acceptance fixtures are examples and are not built
by the normal test command; run a specific fixture with
`cargo run --features interactive-e2e --example ...` on its supported desktop
platform.

For a release sidecar and MCP server:

```text
cargo build --release -p alice-computer -p alice-computer-mcp
```

The binaries will be written to `target/release/`.

## MCP server

Build both binaries, then start:

```text
target/release/alice-computer-mcp --sidecar target/release/alice-computer
```

The MCP server communicates over stdio. Keep stdout reserved for protocol frames;
diagnostics are written to stderr.

The MCP adapter and sidecar start the native desktop runtime lazily. A missing
display or temporarily unavailable macOS permission is returned as a structured
health error and can be retried without restarting the MCP stdio process. A
wait-only `computer_use` batch does not start a desktop session or enumerate
windows.

The server exposes observation and action tools. For a Codex/Computer Use-style
loop, call `computer_use` with an `actions` array containing `click`,
`double_click`, `scroll`, `type`, `wait`, `keypress`, `drag`, `move`, or
`screenshot`. Pointer coordinates are pixels in one current screenshot frame.
The default is automatic: pointer batches and explicit `screenshot` actions
return an image, while keyboard/text-only batches stay compact. Set
`include_screenshot=true` or `false` to force the behavior.

The batch executor keeps two target scopes: pointer/pixel actions stay bound to
the exact window represented by the current screenshot, while ordinary keyboard
and text input is bound to the observed application's process instead of a
stale top-level window. On macOS, `execution_mode` defaults to
`background_preferred`: AX actions and PID-directed input leave the real pointer
and desktop focus alone. Pointer actions are represented by a session-scoped
virtual cursor in returned screenshots; controls without a reliable background
route automatically fall back to foreground takeover. Set `execution_mode` to
`takeover_only` for compatibility. Desktop shortcuts such as `⌘Tab` and
`⌘Space` remain on the global input path; after them, the foreground application,
capability profile, and coordinate frame are refreshed before subsequent actions.

On macOS, display-mode pixel dimensions are used for `DisplayPhysical` scaling,
while each captured frame reports its actual image backing scale. This keeps
Retina and downsampled displays consistent when converting screenshot pixels to
Quartz desktop coordinates.

Each `computer_use` call is limited to 64 actions. In stdio MCP mode both
`computer_use` and `computer_execute` default to an Autonomous broker profile
because stdio cannot service an approval-resume UI. Set
`ALICE_COMPUTER_MCP_REQUIRE_APPROVAL=1` to restore the Conservative approval
profile. Foreground, frame freshness, target security, desktop leases, and
user-interference checks remain active in either mode. Only connect this MCP
server to a trusted local agent, and keep human confirmation in the agent layer
for login, payment, deletion, and other consequential actions.

## Security model

Computer-use software can control the logged-in desktop. Do not expose the sidecar
or MCP process to an untrusted network, and do not bypass the broker in production.

Important protections implemented here include:

- stale frame and stale UI element rejection;
- protected/elevated target classification;
- password-control redaction and value-setting denial;
- foreground-window and generation verification;
- short-lived desktop leases;
- human input/interference detection;
- policy-based approval decisions;
- bounded audit metadata without raw Windows security tokens.

See [SECURITY.md](SECURITY.md) before integrating the project.

## Repository origin

This repository is extracted from Project Alice's independently implemented
computer-use subsystem. It contains no OpenAI, Anthropic, browser-driver, or Tauri
source code. Third-party Rust dependencies remain governed by their own licenses.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
