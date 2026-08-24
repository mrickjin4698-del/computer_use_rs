# Alice Computer Use

[English](README.md) | [简体中文](README.zh-CN.md)

A security-oriented, model-agnostic computer-use runtime for Windows, written in Rust.

The project captures desktop observations, exposes Windows UI Automation elements,
executes semantic or pixel actions, and places a policy/approval broker between an
agent and native input injection. It can be embedded through Rust APIs, run as a
long-lived sidecar, or exposed as a local MCP server.

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
| `alice-computer-use-runtime` | Windows capture, UI Automation, window inspection, and input execution |
| `alice-computer-use-sidecar-protocol` | Length-delimited JSON RPC protocol used by the sidecar |
| `alice-computer-use-sidecar-client` | Host-side process and session client |
| `alice-computer-use-broker` | Leases, policy, approvals, interference detection, audit, and exactly-once dispatch |
| `alice-computer` | Long-lived native sidecar executable |
| `alice-computer-mcp` | Local stdio MCP server exposing the brokered tools |

The repository deliberately excludes the original application's Tauri adapter and
agent-specific prompt/tool integration. Applications are expected to own their
approval UI, lifecycle, and policy configuration.

## Platform support

Windows 10 and Windows 11 are the primary supported platforms. Portable protocol
and policy crates can build elsewhere, but desktop observation and action execution
require Windows APIs.

## Build and test

Install a current stable Rust toolchain, then run:

```powershell
cargo build --workspace
cargo test --workspace
```

For a release sidecar and MCP server:

```powershell
cargo build --release -p alice-computer -p alice-computer-mcp
```

The binaries will be written to `target/release/`.

## MCP server

Build both binaries, then start:

```powershell
target/release/alice-computer-mcp.exe --sidecar target/release/alice-computer.exe
```

The MCP server communicates over stdio. Keep stdout reserved for protocol frames;
diagnostics are written to stderr.

The server exposes observation and action tools. Clients should always observe
before acting, use the newest frame/element references, and observe again after an
uncertain result rather than blindly repeating an action.

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
