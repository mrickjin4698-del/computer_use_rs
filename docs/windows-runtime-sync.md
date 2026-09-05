# Windows runtime optimization sync

This change ports reusable fixes from Project Alice's embedded Rust computer-use
runtime onto `feat/macos-background-computer-use` at `d05e9ea`. The comparison
uses the original open-source extraction (`1cceac4`) as the common baseline.

## Imported behavior

- Window observations include optional Win32 `class_name` metadata. Old JSON
  payloads still deserialize; absent metadata is omitted on serialization.
  macOS emits no Win32 class name. Rust callers constructing `Window` literals
  must add `class_name: None` when unavailable.
- Window enumeration retains the actual foreground HWND during transient
  visibility changes and adds its record if `EnumWindows` omitted it. The
  fallback still requires a successful `GetWindowRect` call.
- Focusing the already foreground window succeeds without another
  `SetForegroundWindow` call.
- The pixel execution path permits a focus action to establish foreground only
  for the admitted target. Ordinary input still requires foreground ownership.
  After dispatch, focus is verified through a fresh window observation:
  `Performed` when active, `VerificationFailed` when inactive, and
  `OutcomeUnknown` when observation fails.
- Windows-key presses and hotkeys append a compact foreground/class diagnostic
  after a 60 ms settling delay. This is an observation, not proof that a
  particular menu opened, and does not replace caller verification.

## Adaptation decisions

The embedded host's `alice-autonomy` task-name exception is not part of the
standalone library. Hosts use the existing explicit approval profile and
background-access settings. Public owner fields retain the generic
`application_session_id` and `agent_thread_id` names.

Class names remain descriptive metadata. The embedded implementation also used
class-name heuristics to infer foreground ownership and relax input admission;
those heuristics are not imported. Generic XAML/application classes do not
uniquely identify a trusted shell window. The actual foreground HWND is used
instead, and shell classification is limited to diagnostic text.

Open-source package metadata and macOS background/takeover behavior are
preserved. Compatibility fixes cover the Windows pixel-intent match after the
addition of `target_application`, platform-specific expectations in the MCP
application-targeting test, and one existing Clippy diagnostic.

## Validation

On Windows:

```text
cargo fmt --all -- --check
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
```

The workspace has 68 passing unit tests. New regressions cover legacy window
JSON, class metadata round-tripping, focus target admission, and diagnostic
classification. Existing policy, interference, coordinate, and MCP tests pass.

Native Start-menu/focus interaction and macOS runtime behavior still require
interactive platform validation; unit tests do not establish those results.
