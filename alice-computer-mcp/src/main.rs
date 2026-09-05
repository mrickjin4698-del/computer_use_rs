//! Standalone MCP adapter for the Alice Computer sidecar.
//!
//! This process owns one Host Broker and one broker session. It exposes only
//! policy-shaped computer tools over MCP stdio. Native runtime, WinNative,
//! UIA, and Windows implementation details stay behind the sidecar boundary.

use alice_computer_use_broker::{
    BrokerError, BrokerExecutionBatchContext, BrokerExecutionRequest, BrokerSessionRef,
    ComputerApprovalProfile, ComputerExecutionBroker, ComputerRequestOwner,
};
use alice_computer_use_core::{
    CaptureFrameMetadata, ComputerAction, ComputerExecutionIntent, ComputerExecutionMode,
    ComputerExecutionOutcome, ComputerExecutionRequest, ComputerExecutionStrategy,
    ComputerFallbackPolicy, ComputerPointerAction, Coordinate, CoordinateSpace, ElementId,
    FrameEncoding, MouseButton, Point, SemanticAction, SemanticObservationLimits, Size, WindowId,
};
use alice_computer_use_sidecar_client::{
    ComputerHostService, ComputerHostServiceConfig, ComputerHostServiceState,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    env,
    io::{self, BufRead, Write},
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "alice-computer-mcp";
const SERVER_VERSION: &str = "0.1.0";

type AdapterResult<T> = Result<T, String>;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

struct McpServer {
    broker: ComputerExecutionBroker,
    owner: ComputerRequestOwner,
    session: Mutex<Option<BrokerSessionRef>>,
    next_request: AtomicU64,
    last_frame: Mutex<Option<CaptureFrameMetadata>>,
}

struct EncodedFrame {
    frame: CaptureFrameMetadata,
    screenshot: alice_computer_use_core::Screenshot,
    cache_hit: bool,
    encode_micros: u128,
}

#[derive(Debug)]
enum ComputerUseStep {
    Actions(Vec<ComputerAction>),
    Wait(Duration),
    Screenshot,
}

impl McpServer {
    fn new(sidecar_path: PathBuf) -> Self {
        let timeout = env::var("ALICE_COMPUTER_MCP_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(30));
        let host = ComputerHostService::new(ComputerHostServiceConfig::new(sidecar_path, timeout));
        let broker = ComputerExecutionBroker::new(host);
        let owner = ComputerRequestOwner::new(
            "mcp-process",
            "mcp-process",
            "mcp-session",
            "mcp-session",
            None,
        );
        Self {
            broker,
            owner,
            session: Mutex::new(None),
            next_request: AtomicU64::new(1),
            last_frame: Mutex::new(None),
        }
    }

    /// Keep MCP stdio alive even when desktop/Accessibility startup is
    /// temporarily unavailable. The first operation that needs a session
    /// retries the Host boundary and returns the concrete broker error.
    fn session(&self) -> Result<BrokerSessionRef, Value> {
        let mut session = self.session.lock().expect("MCP session poisoned");
        if let Some(reference) = session.as_ref() {
            let status = self.broker.status();
            if status.state == ComputerHostServiceState::Ready
                && status.generation == reference.generation
            {
                return Ok(reference.clone());
            }
        }
        let reference = if session.is_some() {
            // A dead/replaced sidecar invalidates every old session. Recovery
            // is read-only and creates a new logical session; no action is
            // ever retried or replayed across the lifecycle boundary.
            self.broker.recover_read_only(self.owner.clone())
        } else {
            self.broker.open_session(self.owner.clone())
        };
        match reference {
            Ok(reference) => {
                *session = Some(reference.clone());
                Ok(reference)
            }
            Err(error) => Err(tool_broker_error(&error)),
        }
    }

    fn handle(&self, request: JsonRpcRequest) -> Option<Value> {
        let id = request.id?;
        let result = match request.method.as_str() {
            "initialize" => Ok(initialize_result()),
            "notifications/initialized" => Ok(json!({})),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tool_call(request.params.unwrap_or_else(|| json!({}))),
            _ => Err(json_rpc_error(
                -32601,
                format!("method not found: {}", request.method),
            )),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        })
    }

    fn handle_tool_call(&self, params: Value) -> Result<Value, Value> {
        let object = params.as_object().ok_or_else(|| {
            tool_error("INVALID_ARGUMENTS", "tools/call params must be an object")
        })?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", "tools/call requires a tool name"))?;
        let arguments = object
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match name {
            "computer_health" => Ok(self.health()),
            "computer_observe" => Ok(self.observe(arguments)),
            "computer_execute" => Ok(self.execute(arguments)),
            "computer_use" => Ok(self.computer_use(arguments)),
            "computer_screenshot" => Ok(self.screenshot(arguments)),
            "computer_validate" => Ok(self.validate(arguments)),
            _ => Err(tool_error(
                "UNKNOWN_TOOL",
                format!("unknown computer tool: {name}"),
            )),
        }
    }

    fn health(&self) -> Value {
        match self.broker.health() {
            Ok(health) => tool_success(
                json!({
                    "ready": health.ready,
                    "initialized": health.initialized,
                    "session_count": health.session_count,
                    "sidecar_pid": health.pid,
                    "session_open": self
                        .session
                        .lock()
                        .expect("MCP session poisoned")
                        .is_some(),
                    "security": health.security,
                    "desktop": health.desktop,
                }),
                "computer sidecar is healthy",
            ),
            Err(error) => tool_broker_error(&error),
        }
    }

    fn observe(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let window_id = match args.optional_string("window_id") {
            Ok(value) => value.map(WindowId::new),
            Err(error) => return error,
        };
        let semantic = match args.boolean("semantic", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let screenshot_metadata = match args.boolean("screenshot_metadata", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let capabilities = match args.boolean("capabilities", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let limits = match args.limits() {
            Ok(value) => value,
            Err(error) => return error,
        };
        if semantic && window_id.is_none() {
            return tool_error(
                "INVALID_ARGUMENTS",
                "semantic=true requires a scoped window_id",
            );
        }
        let session = match self.session() {
            Ok(session) => session,
            Err(error) => return error,
        };

        let observation = if screenshot_metadata {
            match self.broker.observe(&session) {
                Ok(observation) => observation,
                Err(error) => return tool_broker_error(&error),
            }
        } else {
            let windows = match self.broker.window_list(&session) {
                Ok(windows) => windows,
                Err(error) => return tool_broker_error(&error),
            };
            let active_window = windows
                .iter()
                .find(|window| window.active)
                .map(|window| window.id.clone());
            alice_computer_use_core::ComputerObservation {
                session_id: session.computer_session_id.clone(),
                screens: Vec::new(),
                windows,
                active_window,
                screenshot: None,
                frame: None,
                display_topology: None,
            }
        };
        let target_window = window_id.as_ref().and_then(|id| {
            observation
                .windows
                .iter()
                .find(|window| window.id == *id)
                .cloned()
        });
        if window_id.is_some() && target_window.is_none() {
            return tool_error("INVALID_WINDOW", "window_id is not current in this session");
        }

        let capability_profile = if capabilities {
            let Some(window_id) = window_id.as_ref() else {
                return tool_error(
                    "INVALID_ARGUMENTS",
                    "capabilities=true requires a scoped window_id",
                );
            };
            match self.broker.capability_probe(&session, window_id) {
                Ok(profile) => Some(profile),
                Err(error) => return tool_broker_error(&error),
            }
        } else {
            None
        };

        let semantic_observation = if let Some(window_id) = window_id.as_ref().filter(|_| semantic)
        {
            match self.broker.semantic_observe(&session, window_id, limits) {
                Ok(observation) => Some(observation),
                Err(error) => return tool_broker_error(&error),
            }
        } else {
            None
        };
        let generation = semantic_observation
            .as_ref()
            .map(|value| value.metadata.generation);
        let truncated = semantic_observation
            .as_ref()
            .map(|value| value.metadata.truncated)
            .unwrap_or(false);
        let screenshot = observation.frame.as_ref().map(|value| {
            json!({
                "width": value.width,
                "height": value.height,
                "format": "bgra8",
                "coordinate_space": value.coordinate_space,
                "frame_id": value.frame_id,
                "display_id": value.display_id,
                "desktop_origin": value.desktop_origin,
                "topology_generation": value.topology_generation,
                "pixel_format": value.pixel_format,
                "stride": value.stride,
                "dpi": value.dpi,
                "scale": value.scale,
                "pixel_to_desktop_scale": value.pixel_to_desktop_scale,
                "captured_at": value.captured_at,
                "stale_topology": value.stale_topology,
            })
        });
        if let Some(frame) = observation.frame.as_ref() {
            self.remember_frame(frame);
        }
        let structured = json!({
            "session_id": session.computer_session_id,
            "windows": observation.windows,
            "target_window": target_window,
            "display": observation.screens,
            "display_topology": observation.display_topology,
            "semantic": semantic_observation,
            "capability_profile": capability_profile,
            "generation": generation,
            "truncated": truncated,
            "screenshot": screenshot,
            "capabilities": {
                "window_enumeration": true,
                "display_metadata": screenshot_metadata,
                "screenshot": screenshot_metadata,
                "semantic_observation": semantic,
                "execution_policy": true,
            },
        });
        tool_success(structured, "computer observation ready")
    }

    fn execute(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let intent = match args.object("intent") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let operation = match string_field(intent, "operation") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let strategy = match args.strategy() {
            Ok(value) => value,
            Err(error) => return error,
        };
        let fallback_policy = match args.fallback_policy() {
            Ok(value) => value,
            Err(error) => return error,
        };
        let execution_mode = match args.execution_mode() {
            Ok(value) => value,
            Err(error) => return error,
        };
        let intent = match operation.as_str() {
            "focus_window" => {
                let target_window_id = match string_field(intent, "target_window_id") {
                    Ok(value) => WindowId::new(value),
                    Err(error) => return error,
                };
                ComputerExecutionIntent::Pixel {
                    action: ComputerAction::FocusWindow {
                        window_id: target_window_id.clone(),
                    },
                    target_window_id: Some(target_window_id),
                    target_application: None,
                }
            }
            "focus" | "invoke" | "set_value" | "toggle" | "select" | "expand" | "collapse"
            | "set_range_value" | "scroll_into_view" => {
                let element_id = match string_field(intent, "element_id") {
                    Ok(value) => ElementId::new(value),
                    Err(error) => return error,
                };
                let action = match operation.as_str() {
                    "focus" => SemanticAction::Focus { element_id },
                    "invoke" => SemanticAction::Invoke { element_id },
                    "set_value" => {
                        let value = match string_field(intent, "value") {
                            Ok(value) => value,
                            Err(error) => return error,
                        };
                        SemanticAction::SetValue { element_id, value }
                    }
                    "toggle" => SemanticAction::Toggle { element_id },
                    "select" => SemanticAction::Select { element_id },
                    "expand" => SemanticAction::Expand { element_id },
                    "collapse" => SemanticAction::Collapse { element_id },
                    "set_range_value" => {
                        let value = match number_field(intent, "value") {
                            Ok(value) => value,
                            Err(error) => return error,
                        };
                        SemanticAction::SetRangeValue { element_id, value }
                    }
                    "scroll_into_view" => SemanticAction::ScrollIntoView { element_id },
                    _ => unreachable!(),
                };
                ComputerExecutionIntent::Semantic(action)
            }
            "mouse_down" | "mouse_up" | "middle_click" | "triple_click" | "modifier_click"
            | "key_down" | "key_up" | "hold_key" => {
                let target_window_id = match string_field(intent, "target_window_id") {
                    Ok(value) => WindowId::new(value),
                    Err(error) => return error,
                };
                let at = if matches!(
                    operation.as_str(),
                    "mouse_down" | "mouse_up" | "middle_click" | "triple_click" | "modifier_click"
                ) {
                    match coordinate_field(intent, "at") {
                        Ok(value) => Some(value),
                        Err(error) => return error,
                    }
                } else {
                    None
                };
                let action = match operation.as_str() {
                    "mouse_down" => ComputerAction::MouseDown {
                        button: match mouse_button_field(intent, "button") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        at: at.expect("mouse_down coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "mouse_up" => ComputerAction::MouseUp {
                        button: match mouse_button_field(intent, "button") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        at: at.expect("mouse_up coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "middle_click" => ComputerAction::MiddleClick {
                        at: at.expect("middle_click coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "triple_click" => ComputerAction::TripleClick {
                        at: at.expect("triple_click coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "modifier_click" => ComputerAction::ModifierClick {
                        modifier: match string_field(intent, "modifier") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        button: match mouse_button_field(intent, "button") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        at: at.expect("modifier_click coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "key_down" => ComputerAction::KeyDown {
                        key: match string_field(intent, "key") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        target: Some(target_window_id.clone()),
                    },
                    "key_up" => ComputerAction::KeyUp {
                        key: match string_field(intent, "key") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        target: Some(target_window_id.clone()),
                    },
                    "hold_key" => ComputerAction::HoldKey {
                        key: match string_field(intent, "key") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        duration_ms: match number_u32_field(intent, "duration_ms") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        target: Some(target_window_id.clone()),
                    },
                    _ => unreachable!(),
                };
                ComputerExecutionIntent::Pixel {
                    action,
                    target_window_id: Some(target_window_id),
                    target_application: None,
                }
            }
            _ => return tool_error("INVALID_ARGUMENTS", "unsupported intent.operation"),
        };
        let request = ComputerExecutionRequest {
            intent,
            strategy,
            fallback_policy,
            execution_mode,
        };
        let session = match self.session() {
            Ok(session) => session,
            Err(error) => return error,
        };
        let request_id = format!("mcp-{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        let lease = match self.broker.acquire_desktop_lease(&session) {
            Ok(lease) => lease,
            Err(error) => return tool_broker_error(&error),
        };
        // This stdio adapter has no interactive approval-resume channel. Keep
        // the local MCP test path functional by resolving the approval profile
        // to Autonomous by default; ALICE_COMPUTER_MCP_REQUIRE_APPROVAL=1
        // restores the host approval profile when this adapter is deployed in
        // a guarded environment. Capability, foreground, lease, input-monitor,
        // and security gates remain enforced in either mode.
        let result = self.broker.execute_with_profile(
            BrokerExecutionRequest {
                request_id,
                session,
                lease: lease.clone(),
                execution: request,
            },
            mcp_execution_profile(),
        );
        let _ = self.broker.release_desktop_lease(&lease);
        match result {
            Ok(result) => execution_tool_result(result),
            Err(error) => tool_broker_error(&error),
        }
    }

    fn screenshot(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let screen_id = match args.optional_string("screen_id") {
            Ok(value) => value.map(alice_computer_use_core::ScreenId::new),
            Err(error) => return error,
        };
        let session = match self.session() {
            Ok(session) => session,
            Err(error) => return error,
        };
        match self.broker.capture_frame(&session, screen_id) {
            Ok(frame) => {
                match self
                    .broker
                    .encode_frame(&session, &frame.frame_id, FrameEncoding::Png)
                {
                    Ok(encoded) => {
                        let screenshot = encoded.screenshot;
                        self.remember_frame(&frame);
                        let metadata = json!({
                            "width": screenshot.metadata.width,
                            "height": screenshot.metadata.height,
                            "format": screenshot.metadata.mime_type,
                            "coordinate_space": screenshot.metadata.coordinate_space,
                            "frame_id": screenshot.metadata.frame_id,
                            "display_id": screenshot.metadata.display_id,
                            "desktop_origin": screenshot.metadata.desktop_origin,
                            "dpi": screenshot.metadata.dpi,
                            "scale": screenshot.metadata.scale,
                            "captured_at": screenshot.metadata.captured_at,
                            "bytes": screenshot.bytes.len(),
                            "encode_cache_hit": encoded.cache_hit,
                            "encode_micros": encoded.encode_micros,
                            "topology_generation": frame.topology_generation,
                            "pixel_format": frame.pixel_format,
                            "stride": frame.stride,
                            "stale_topology": frame.stale_topology,
                        });
                        let content = json!([
                            { "type": "text", "text": metadata.to_string() },
                            {
                                "type": "image",
                                "data": BASE64.encode(&screenshot.bytes),
                                "mimeType": screenshot.metadata.mime_type,
                            }
                        ]);
                        json!({
                            "content": content,
                            "structuredContent": { "metadata": metadata },
                            "isError": false,
                        })
                    }
                    Err(error) => tool_broker_error(&error),
                }
            }
            Err(error) => tool_broker_error(&error),
        }
    }

    /// Execute an OpenAI computer-use-shaped action batch. The broker still
    /// owns foreground, capability, lease, security, and input-monitor gates;
    /// the request-scoped autonomous profile only removes an MCP approval UI
    /// dependency that this stdio adapter cannot service interactively.
    fn computer_use(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let actions = match args.array("actions") {
            Ok(actions) => actions,
            Err(error) => return error,
        };
        if actions.is_empty() {
            return tool_error(
                "INVALID_ARGUMENTS",
                "actions must contain at least one action",
            );
        }
        if actions.len() > 64 {
            return tool_error(
                "INVALID_ARGUMENTS",
                "actions is limited to 64 items per call",
            );
        }

        let target_window = match args.optional_string("window_id") {
            Ok(value) => value.map(WindowId::new),
            Err(error) => return error,
        };

        let include_screenshot = match args.value.get("include_screenshot") {
            None => None,
            Some(value) => match value.as_bool() {
                Some(value) => Some(value),
                None => {
                    return tool_error("INVALID_ARGUMENTS", "include_screenshot must be boolean")
                }
            },
        };
        let include_action_details = match args.boolean("include_action_details", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let execution_mode = match args.execution_mode() {
            Ok(value) => value,
            Err(error) => return error,
        };
        let needs_execution = actions.iter().any(action_requires_execution);
        let needs_coordinate_frame = actions.iter().any(action_requires_frame);
        let wants_screenshot = include_screenshot.unwrap_or(needs_coordinate_frame)
            || actions
                .iter()
                .any(|action| action.get("type").and_then(Value::as_str) == Some("screenshot"));
        if !needs_execution {
            let mut results = Vec::with_capacity(actions.len());
            for (index, action_value) in actions.iter().enumerate() {
                let step = match parse_computer_use_action(action_value, None, None) {
                    Ok(step) => step,
                    Err(error) => {
                        return tool_error(
                            "INVALID_ACTION",
                            format!("actions[{index}]: {}", tool_error_message(error)),
                        )
                    }
                };
                match step {
                    ComputerUseStep::Wait(duration) => {
                        std::thread::sleep(duration);
                        results.push(json!({
                            "index": index,
                            "type": "wait",
                            "status": "performed",
                            "duration_ms": duration.as_millis(),
                        }));
                    }
                    ComputerUseStep::Screenshot => {
                        results.push(json!({
                            "index": index,
                            "type": "screenshot",
                            "status": "deferred_to_batch_output",
                        }));
                    }
                    ComputerUseStep::Actions(_) => {
                        return tool_error(
                            "INVALID_ACTION",
                            format!("actions[{index}] unexpectedly requires execution"),
                        )
                    }
                }
            }
            let output = if wants_screenshot {
                match self.capture_encoded_frame(None) {
                    Ok(output) => Some(output),
                    Err(error) => return error,
                }
            } else {
                None
            };
            return self.computer_use_output(results, None, None, output);
        }
        let session = match self.session() {
            Ok(session) => session,
            Err(error) => return error,
        };
        let lease = match self.broker.acquire_desktop_lease(&session) {
            Ok(lease) => lease,
            Err(error) => return tool_broker_error(&error),
        };
        let requested_target = target_window.clone();
        let probe_capability = needs_execution
            && !actions
                .iter()
                .all(action_value_is_transient_dismissal_or_wait);
        let mut batch_context = match self.broker.prepare_execution_batch(
            &session,
            &lease,
            requested_target.clone(),
            probe_capability,
            background_execution_enabled(execution_mode),
        ) {
            Ok(context) => context,
            Err(error) => {
                let _ = self.broker.release_desktop_lease(&lease);
                return match error.code() {
                    "TARGET_FOREGROUND_CHANGED" => {
                        tool_error("FOREGROUND_REQUIRED", error.to_string())
                    }
                    "INVALID_REQUEST" => tool_error("INVALID_WINDOW", error.to_string()),
                    _ => tool_broker_error(&error),
                };
            }
        };
        let mut target_id = batch_context.target_window_id();
        // Keyboard/text-only batches do not need a screenshot just to dispatch
        // input. Pointer actions are still bound to one current frame; their
        // final PNG is kept by default for the next visual turn, while pure
        // keyboard/text batches stay compact unless explicitly requested.
        let mut frame = if needs_coordinate_frame {
            match self.capture_current_frame() {
                Ok(frame) => Some(frame),
                Err(error) => {
                    let _ = self.broker.release_desktop_lease(&lease);
                    return error;
                }
            }
        } else {
            None
        };
        // Some macOS focus surfaces (notably Spotlight and the app switcher)
        // are transient system overlays and do not appear as AX windows. Keep
        // following keyboard/text actions on the global event path until a
        // later foreground observation proves that a new application window
        // has appeared.
        let mut global_input_pending = false;
        let mut foreground_transition_committed = false;
        let mut results = Vec::with_capacity(actions.len());

        for (index, action_value) in actions.iter().enumerate() {
            let step =
                match parse_computer_use_action(action_value, frame.as_ref(), target_id.clone()) {
                    Ok(step) => step,
                    Err(error) => {
                        let _ = self.broker.release_desktop_lease(&lease);
                        return tool_error(
                            "INVALID_ACTION",
                            format!("actions[{index}]: {}", tool_error_message(error)),
                        );
                    }
                };
            match step {
                ComputerUseStep::Wait(duration) => {
                    std::thread::sleep(duration);
                    if global_input_pending {
                        let previous_target = target_id.clone();
                        match self.rebind_batch_context(
                            &session,
                            &lease,
                            requested_target.clone(),
                            needs_execution,
                            previous_target.as_ref(),
                            execution_mode,
                        ) {
                            Ok(context) => {
                                batch_context = context;
                                target_id = batch_context.target_window_id();
                                if foreground_transition_committed {
                                    global_input_pending = target_id == previous_target;
                                }
                                if needs_coordinate_frame && !global_input_pending {
                                    match self.capture_current_frame() {
                                        Ok(new_frame) => frame = Some(new_frame),
                                        Err(error) => {
                                            let _ = self.broker.release_desktop_lease(&lease);
                                            return error;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = self.broker.release_desktop_lease(&lease);
                                return error;
                            }
                        }
                    }
                    results.push(json!({
                        "index": index,
                        "type": "wait",
                        "status": "performed",
                        "duration_ms": duration.as_millis(),
                    }));
                }
                ComputerUseStep::Screenshot => {
                    if needs_coordinate_frame {
                        match self.capture_current_frame() {
                            Ok(new_frame) => frame = Some(new_frame),
                            Err(error) => {
                                let _ = self.broker.release_desktop_lease(&lease);
                                return error;
                            }
                        }
                    }
                    results.push(json!({
                        "index": index,
                        "type": "screenshot",
                        "status": "deferred_to_batch_output",
                    }));
                }
                ComputerUseStep::Actions(step_actions) => {
                    for action in step_actions {
                        let request_id = format!(
                            "mcp-computer-use-{}",
                            self.next_request.fetch_add(1, Ordering::Relaxed)
                        );
                        let global_scope = global_input_pending
                            || action_changes_foreground(&action)
                            || (requested_target.is_none()
                                && action_is_transient_dismissal(&action));
                        let dispatch_action = if global_scope {
                            clear_window_target(action.clone())
                        } else {
                            action.clone()
                        };
                        let request = batch_execution_request(
                            dispatch_action,
                            if global_scope {
                                None
                            } else {
                                target_id.clone()
                            },
                            if global_scope {
                                None
                            } else {
                                batch_context.target_application()
                            },
                            global_scope,
                            execution_mode,
                        );
                        let result = self.broker.execute_with_profile_in_batch(
                            BrokerExecutionRequest {
                                request_id,
                                session: session.clone(),
                                lease: lease.clone(),
                                execution: request,
                            },
                            ComputerApprovalProfile::Autonomous,
                            &batch_context,
                        );
                        match result {
                            Ok(result) => {
                                let outcome = result.final_outcome;
                                if include_action_details {
                                    results.push(json!({
                                        "index": index,
                                        "type": action_type_name(&action),
                                        "outcome": outcome,
                                        "result": result,
                                    }));
                                } else {
                                    results.push(json!({
                                        "index": index,
                                        "type": action_type_name(&action),
                                        "outcome": outcome,
                                    }));
                                }
                                if outcome != ComputerExecutionOutcome::Performed {
                                    let _ = self.broker.release_desktop_lease(&lease);
                                    return computer_use_error_with_results(
                                        "ACTION_FAILED",
                                        format!("actions[{index}] returned {outcome:?}"),
                                        results,
                                    );
                                }
                                let starts_foreground_transition =
                                    action_changes_foreground(&action);
                                let commits_foreground_transition = global_input_pending
                                    && action_commits_foreground_transition(&action);
                                if requested_target.is_none()
                                    && (starts_foreground_transition
                                        || commits_foreground_transition)
                                {
                                    // Spotlight is a transient system overlay,
                                    // not a normal AX window. Do not rebind to
                                    // an incidental window while it is open;
                                    // keep the next text/key action global.
                                    if starts_foreground_transition
                                        && action_opens_transient_overlay(&action)
                                    {
                                        global_input_pending = true;
                                        foreground_transition_committed = false;
                                        continue;
                                    }
                                    let previous_target = target_id.clone();
                                    match self.rebind_batch_context(
                                        &session,
                                        &lease,
                                        requested_target.clone(),
                                        needs_execution,
                                        target_id.as_ref(),
                                        execution_mode,
                                    ) {
                                        Ok(context) => {
                                            batch_context = context;
                                            target_id = batch_context.target_window_id();
                                            if starts_foreground_transition {
                                                global_input_pending = false;
                                                foreground_transition_committed = false;
                                            } else {
                                                foreground_transition_committed = true;
                                                global_input_pending = target_id == previous_target;
                                            }
                                            if needs_coordinate_frame {
                                                match self.capture_current_frame() {
                                                    Ok(new_frame) => frame = Some(new_frame),
                                                    Err(error) => {
                                                        let _ = self
                                                            .broker
                                                            .release_desktop_lease(&lease);
                                                        return error;
                                                    }
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            let _ = self.broker.release_desktop_lease(&lease);
                                            return error;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = self.broker.release_desktop_lease(&lease);
                                return computer_use_error_with_results(
                                    error.code(),
                                    format!("actions[{index}]: {error}"),
                                    results,
                                );
                            }
                        }
                    }
                }
            }
        }
        let _ = self.broker.release_desktop_lease(&lease);

        let output = if wants_screenshot {
            match self.capture_encoded_frame(None) {
                Ok(output) => Some(output),
                Err(error) => return error,
            }
        } else {
            None
        };
        self.computer_use_output(results, frame.as_ref(), target_id, output)
    }

    /// Rebind only after a bounded action that can change the foreground
    /// application.  The lease and logical session remain the same, but the
    /// window/capability snapshot is refreshed so the next action cannot be
    /// sent through a stale top-level window id.
    fn rebind_batch_context(
        &self,
        session: &BrokerSessionRef,
        lease: &alice_computer_use_broker::DesktopLeaseRef,
        requested_target: Option<WindowId>,
        probe_capability: bool,
        previous_target: Option<&WindowId>,
        execution_mode: ComputerExecutionMode,
    ) -> Result<BrokerExecutionBatchContext, Value> {
        const REBIND_ATTEMPTS: usize = 5;
        const REBIND_DELAY: Duration = Duration::from_millis(40);
        for attempt in 0..REBIND_ATTEMPTS {
            match self.broker.prepare_execution_batch(
                session,
                lease,
                requested_target.clone(),
                false,
                background_execution_enabled(execution_mode),
            ) {
                Ok(context)
                    if requested_target.is_some()
                        || context.target_window_id().as_ref() != previous_target
                        || attempt + 1 == REBIND_ATTEMPTS =>
                {
                    if probe_capability {
                        return self
                            .broker
                            .prepare_execution_batch(
                                session,
                                lease,
                                requested_target.clone(),
                                true,
                                background_execution_enabled(execution_mode),
                            )
                            .map_err(|error| tool_broker_error(&error));
                    }
                    return Ok(context);
                }
                Ok(_) => std::thread::sleep(REBIND_DELAY),
                Err(error) if attempt + 1 < REBIND_ATTEMPTS => {
                    std::thread::sleep(REBIND_DELAY);
                    if requested_target.is_some() {
                        return Err(tool_broker_error(&error));
                    }
                }
                Err(error) => return Err(tool_broker_error(&error)),
            }
        }
        unreachable!("rebind attempts always return or sleep")
    }

    fn computer_use_output(
        &self,
        results: Vec<Value>,
        frame: Option<&CaptureFrameMetadata>,
        target_id: Option<WindowId>,
        output: Option<EncodedFrame>,
    ) -> Value {
        let metadata = output.as_ref().map(|output| {
            frame_metadata_json(
                &output.frame,
                &output.screenshot,
                output.cache_hit,
                output.encode_micros,
            )
        });
        let structured = json!({
            "actions": results,
            "screenshot": metadata,
            "coordinate_frame_id": frame.as_ref().map(|frame| frame.frame_id.clone()),
            "result_frame_id": output.as_ref().map(|output| output.frame.frame_id.clone()),
            "window_id": target_id,
        });
        let mut content = vec![json!({
            "type": "text",
            "text": structured.to_string(),
        })];
        if let Some(output) = output {
            content.push(json!({
                "type": "image",
                "data": BASE64.encode(&output.screenshot.bytes),
                "mimeType": output.screenshot.metadata.mime_type,
            }));
        }
        json!({
            "content": content,
            "structuredContent": structured,
            "isError": false,
        })
    }

    fn capture_current_frame(&self) -> Result<CaptureFrameMetadata, Value> {
        let session = self.session()?;
        match self.broker.capture_frame(&session, None) {
            Ok(frame) => {
                self.remember_frame(&frame);
                Ok(frame)
            }
            Err(error) => Err(tool_broker_error(&error)),
        }
    }

    fn capture_encoded_frame(
        &self,
        screen_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<EncodedFrame, Value> {
        let session = self.session()?;
        let frame = match self.broker.capture_frame(&session, screen_id) {
            Ok(frame) => frame,
            Err(error) => return Err(tool_broker_error(&error)),
        };
        self.remember_frame(&frame);
        let encoded = match self
            .broker
            .encode_frame(&session, &frame.frame_id, FrameEncoding::Png)
        {
            Ok(encoded) => encoded,
            Err(error) => return Err(tool_broker_error(&error)),
        };
        Ok(EncodedFrame {
            frame,
            screenshot: encoded.screenshot,
            cache_hit: encoded.cache_hit,
            encode_micros: encoded.encode_micros,
        })
    }

    fn remember_frame(&self, frame: &CaptureFrameMetadata) {
        if let Ok(mut current) = self.last_frame.lock() {
            *current = Some(frame.clone());
        }
    }

    fn validate(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let element_id = match args.optional_string("element_id") {
            Ok(value) => value.map(ElementId::new),
            Err(error) => return error,
        };
        let window_id = match args.optional_string("window_id") {
            Ok(value) => value.map(WindowId::new),
            Err(error) => return error,
        };
        if element_id.is_none() && window_id.is_none() {
            return tool_error("INVALID_ARGUMENTS", "element_id or window_id is required");
        }
        if let Some(window_id) = window_id {
            let session = match self.session() {
                Ok(session) => session,
                Err(error) => return error,
            };
            match self
                .broker
                .window_list(&session)
                .map(|windows| windows.into_iter().any(|window| window.id == window_id))
            {
                Ok(true) => {}
                Ok(false) => return tool_error("INVALID_WINDOW", "window is not current"),
                Err(error) => return tool_broker_error(&error),
            }
        }
        if let Some(element_id) = element_id {
            let session = match self.session() {
                Ok(session) => session,
                Err(error) => return error,
            };
            match self.broker.validate_element(&session, &element_id) {
                Ok(()) => tool_success(
                    json!({ "valid": true, "element_id": element_id }),
                    "element is current",
                ),
                Err(error) => tool_broker_error(&error),
            }
        } else {
            tool_success(json!({ "valid": true }), "window is current")
        }
    }

    fn shutdown(&self) {
        let _ = self.broker.shutdown();
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ObjectArgs {
    value: Map<String, Value>,
}

impl ObjectArgs {
    fn new(value: Value) -> Result<Self, Value> {
        value
            .as_object()
            .cloned()
            .map(|value| Self { value })
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", "tool arguments must be an object"))
    }

    fn object(&self, name: &str) -> Result<&Map<String, Value>, Value> {
        self.value
            .get(name)
            .and_then(Value::as_object)
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be an object")))
    }

    fn optional_string(&self, name: &str) -> Result<Option<String>, Value> {
        match self.value.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => {
                value.as_str().map(str::to_owned).map(Some).ok_or_else(|| {
                    tool_error("INVALID_ARGUMENTS", format!("{name} must be a string"))
                })
            }
        }
    }

    fn array(&self, name: &str) -> Result<&Vec<Value>, Value> {
        self.value
            .get(name)
            .and_then(Value::as_array)
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be an array")))
    }

    fn boolean(&self, name: &str, default: bool) -> Result<bool, Value> {
        match self.value.get(name) {
            None => Ok(default),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be boolean"))),
        }
    }

    fn u32(&self, name: &str, default: u32) -> Result<u32, Value> {
        match self.value.get(name) {
            None => Ok(default),
            Some(value) => value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be uint32"))),
        }
    }

    fn limits(&self) -> Result<SemanticObservationLimits, Value> {
        Ok(SemanticObservationLimits {
            // Keep the default semantic snapshot small enough for an agent
            // turn; callers can request a larger bounded view when the target
            // is not present in the compact tree.
            max_depth: self.u32("max_depth", 6)?,
            max_elements: self.u32("max_elements", 256)?,
        }
        .bounded())
    }

    fn strategy(&self) -> Result<ComputerExecutionStrategy, Value> {
        match self
            .value
            .get("strategy")
            .and_then(Value::as_str)
            .unwrap_or("prefer_semantic")
        {
            "semantic_only" => Ok(ComputerExecutionStrategy::SemanticOnly),
            "pixel_only" => Ok(ComputerExecutionStrategy::PixelOnly),
            "prefer_semantic" => Ok(ComputerExecutionStrategy::PreferSemantic),
            "prefer_pixel" => Ok(ComputerExecutionStrategy::PreferPixel),
            value => Err(tool_error(
                "INVALID_ARGUMENTS",
                format!("unsupported strategy: {value}"),
            )),
        }
    }

    fn fallback_policy(&self) -> Result<ComputerFallbackPolicy, Value> {
        match self
            .value
            .get("fallback_policy")
            .and_then(Value::as_str)
            .unwrap_or("deny")
        {
            "allow" => Ok(ComputerFallbackPolicy::Allow),
            "deny" => Ok(ComputerFallbackPolicy::Deny),
            "require_explicit" => Ok(ComputerFallbackPolicy::RequireExplicit),
            value => Err(tool_error(
                "INVALID_ARGUMENTS",
                format!("unsupported fallback_policy: {value}"),
            )),
        }
    }

    fn execution_mode(&self) -> Result<ComputerExecutionMode, Value> {
        match self
            .value
            .get("execution_mode")
            .and_then(Value::as_str)
            .unwrap_or("background_preferred")
        {
            "background_preferred" => Ok(ComputerExecutionMode::BackgroundPreferred),
            "takeover_only" => Ok(ComputerExecutionMode::TakeoverOnly),
            value => Err(tool_error(
                "INVALID_ARGUMENTS",
                format!("unsupported execution_mode: {value}"),
            )),
        }
    }
}

fn string_field(object: &Map<String, Value>, name: &str) -> Result<String, Value> {
    object
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} is required")))
}

fn number_field(object: &Map<String, Value>, name: &str) -> Result<f64, Value> {
    object
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} must be number")))
}

fn number_u32_field(object: &Map<String, Value>, name: &str) -> Result<u32, Value> {
    object
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} must be uint32")))
}

fn coordinate_field(object: &Map<String, Value>, name: &str) -> Result<Coordinate, Value> {
    object
        .get(name)
        .cloned()
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} is required")))
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| {
                tool_error(
                    "INVALID_ARGUMENTS",
                    format!("intent.{name} is not a valid coordinate: {error}"),
                )
            })
        })
}

fn mouse_button_field(object: &Map<String, Value>, name: &str) -> Result<MouseButton, Value> {
    match object.get(name).and_then(Value::as_str).unwrap_or("left") {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        value => Err(tool_error(
            "INVALID_ARGUMENTS",
            format!("intent.{name} must be left, right, or middle; got {value}"),
        )),
    }
}

fn parse_computer_use_action(
    action: &Value,
    frame: Option<&CaptureFrameMetadata>,
    target: Option<WindowId>,
) -> Result<ComputerUseStep, Value> {
    let object = action
        .as_object()
        .ok_or_else(|| tool_error("INVALID_ACTION", "computer-use action must be an object"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| tool_error("INVALID_ACTION", "computer-use action.type is required"))?;
    match kind {
        "wait" => {
            let duration_ms = object
                .get("duration_ms")
                .map(|value| integer_u32_value(value, "duration_ms"))
                .transpose()?
                .unwrap_or(1_000)
                .min(10_000);
            Ok(ComputerUseStep::Wait(Duration::from_millis(
                duration_ms as u64,
            )))
        }
        "screenshot" => Ok(ComputerUseStep::Screenshot),
        "type" => {
            let text = string_value(object, "text")?;
            Ok(ComputerUseStep::Actions(vec![ComputerAction::TypeText {
                text,
                target,
                at: None,
            }]))
        }
        "keypress" => {
            let keys = key_array(object, "keys")?;
            if keys.is_empty() {
                return Err(tool_error(
                    "INVALID_ACTION",
                    "keypress.keys must contain at least one key",
                ));
            }
            Ok(ComputerUseStep::Actions(vec![ComputerAction::Hotkey {
                keys,
                target,
            }]))
        }
        "click" => {
            let coordinate = screenshot_coordinate(object, frame)?;
            let button = mouse_button_value(object, "button")?;
            let keys = key_array(object, "keys")?;
            let action = if keys.is_empty() {
                match button {
                    MouseButton::Left => ComputerAction::Click { at: coordinate },
                    MouseButton::Right => ComputerAction::RightClick { at: coordinate },
                    MouseButton::Middle => ComputerAction::MiddleClick {
                        at: coordinate,
                        target: target.clone(),
                    },
                }
            } else {
                ComputerAction::ModifiedPointer {
                    action: ComputerPointerAction::Click {
                        at: coordinate,
                        button,
                        clicks: 1,
                    },
                    modifiers: keys,
                }
            };
            Ok(ComputerUseStep::Actions(vec![action]))
        }
        "double_click" => {
            let coordinate = screenshot_coordinate(object, frame)?;
            let button = mouse_button_value(object, "button")?;
            let keys = key_array(object, "keys")?;
            if keys.is_empty() && button != MouseButton::Left {
                return Err(tool_error(
                    "UNSUPPORTED_ACTION",
                    "double_click without modifiers supports the left button only",
                ));
            }
            let action = if keys.is_empty() {
                ComputerAction::DoubleClick { at: coordinate }
            } else {
                ComputerAction::ModifiedPointer {
                    action: ComputerPointerAction::Click {
                        at: coordinate,
                        button,
                        clicks: 2,
                    },
                    modifiers: keys,
                }
            };
            Ok(ComputerUseStep::Actions(vec![action]))
        }
        "move" => {
            let coordinate = screenshot_coordinate(object, frame)?;
            let keys = key_array(object, "keys")?;
            let action = if keys.is_empty() {
                ComputerAction::MovePointer { to: coordinate }
            } else {
                ComputerAction::ModifiedPointer {
                    action: ComputerPointerAction::Move { to: coordinate },
                    modifiers: keys,
                }
            };
            Ok(ComputerUseStep::Actions(vec![action]))
        }
        "scroll" => {
            let coordinate = screenshot_coordinate(object, frame)?;
            let scroll_x = integer_i32_value(
                object
                    .get("scroll_x")
                    .ok_or_else(|| tool_error("INVALID_ACTION", "scroll.scroll_x is required"))?,
                "scroll_x",
            )?;
            let scroll_y = integer_i32_value(
                object
                    .get("scroll_y")
                    .ok_or_else(|| tool_error("INVALID_ACTION", "scroll.scroll_y is required"))?,
                "scroll_y",
            )?;
            if scroll_x == 0 && scroll_y == 0 {
                return Err(tool_error(
                    "INVALID_ACTION",
                    "scroll must move at least one axis",
                ));
            }
            let keys = key_array(object, "keys")?;
            let action = if keys.is_empty() && (scroll_x == 0 || scroll_y == 0) {
                let (direction, amount) = if scroll_y != 0 {
                    (
                        if scroll_y < 0 {
                            alice_computer_use_core::ScrollDirection::Up
                        } else {
                            alice_computer_use_core::ScrollDirection::Down
                        },
                        scroll_y.unsigned_abs(),
                    )
                } else {
                    (
                        if scroll_x < 0 {
                            alice_computer_use_core::ScrollDirection::Left
                        } else {
                            alice_computer_use_core::ScrollDirection::Right
                        },
                        scroll_x.unsigned_abs(),
                    )
                };
                ComputerAction::Scroll {
                    at: coordinate,
                    direction,
                    amount,
                }
            } else {
                ComputerAction::ModifiedPointer {
                    action: ComputerPointerAction::Scroll {
                        at: coordinate,
                        delta_x: scroll_x,
                        delta_y: scroll_y,
                    },
                    modifiers: keys,
                }
            };
            Ok(ComputerUseStep::Actions(vec![action]))
        }
        "drag" => {
            let keys = key_array(object, "keys")?;
            let path = object
                .get("path")
                .and_then(Value::as_array)
                .ok_or_else(|| tool_error("INVALID_ACTION", "drag.path is required"))?;
            if path.len() < 2 {
                return Err(tool_error(
                    "INVALID_ACTION",
                    "drag.path must contain at least two points",
                ));
            }
            let path = path
                .iter()
                .map(|point| screenshot_coordinate_value(point, frame))
                .collect::<Result<Vec<_>, _>>()?;
            let button = mouse_button_value(object, "button")?;
            let action = if keys.is_empty() && path.len() == 2 {
                ComputerAction::Drag {
                    from: path[0].clone(),
                    to: path[1].clone(),
                    button,
                }
            } else {
                ComputerAction::ModifiedPointer {
                    action: ComputerPointerAction::Drag { path, button },
                    modifiers: keys,
                }
            };
            Ok(ComputerUseStep::Actions(vec![action]))
        }
        _ => Err(tool_error(
            "UNSUPPORTED_ACTION",
            format!("unsupported computer-use action type: {kind}"),
        )),
    }
}

fn action_requires_frame(action: &Value) -> bool {
    matches!(
        action.get("type").and_then(Value::as_str),
        Some("click" | "double_click" | "scroll" | "drag" | "move")
    )
}

fn batch_execution_request(
    action: ComputerAction,
    target_window: Option<WindowId>,
    target_application: Option<alice_computer_use_core::ApplicationIdentity>,
    global_input_pending: bool,
    execution_mode: ComputerExecutionMode,
) -> ComputerExecutionRequest {
    if cfg!(target_os = "macos") && !global_input_pending {
        if let Some(application) = target_application.filter(|value| value.process_id.is_some()) {
            if application_scoped_input(&action) && !action_changes_foreground(&action) {
                return ComputerExecutionRequest::application(action, application)
                    .with_execution_mode(execution_mode);
            }
        }
    }
    ComputerExecutionRequest::pixel(action, target_window).with_execution_mode(execution_mode)
}

fn background_execution_enabled(mode: ComputerExecutionMode) -> bool {
    cfg!(target_os = "macos") && mode == ComputerExecutionMode::BackgroundPreferred
}

fn clear_window_target(action: ComputerAction) -> ComputerAction {
    match action {
        ComputerAction::TypeText { text, at, .. } => ComputerAction::TypeText {
            text,
            target: None,
            at,
        },
        ComputerAction::KeyPress { key, .. } => ComputerAction::KeyPress { key, target: None },
        ComputerAction::Hotkey { keys, .. } => ComputerAction::Hotkey { keys, target: None },
        ComputerAction::KeyDown { key, .. } => ComputerAction::KeyDown { key, target: None },
        ComputerAction::KeyUp { key, .. } => ComputerAction::KeyUp { key, target: None },
        ComputerAction::HoldKey {
            key, duration_ms, ..
        } => ComputerAction::HoldKey {
            key,
            duration_ms,
            target: None,
        },
        other => other,
    }
}

fn application_scoped_input(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::TypeText { at: None, .. }
            | ComputerAction::KeyPress { .. }
            | ComputerAction::Hotkey { .. }
            | ComputerAction::KeyDown { .. }
            | ComputerAction::KeyUp { .. }
            | ComputerAction::HoldKey { .. }
    )
}

fn action_changes_foreground(action: &ComputerAction) -> bool {
    let keys = match action {
        ComputerAction::Hotkey { keys, .. } => keys,
        _ => return false,
    };
    let has = |key: &str| keys.iter().any(|value| value == key);
    // These shortcuts are handled by the desktop/window manager.  They must
    // remain global; subsequent actions are rebound to the newly foreground
    // application instead of being posted to the old PID.
    (has("meta") && (has("tab") || has("space") || has("w") || has("q")))
        || (has("alt") && has("tab"))
}

fn action_opens_transient_overlay(action: &ComputerAction) -> bool {
    let ComputerAction::Hotkey { keys, .. } = action else {
        return false;
    };
    let has = |key: &str| keys.iter().any(|value| value == key);
    has("meta") && has("space")
}

fn action_is_transient_dismissal(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::KeyPress { key, .. }
            if key.eq_ignore_ascii_case("escape") || key.eq_ignore_ascii_case("esc")
    ) || matches!(
        action,
        ComputerAction::Hotkey { keys, .. }
            if keys.len() == 1
                && (keys[0].eq_ignore_ascii_case("escape")
                    || keys[0].eq_ignore_ascii_case("esc"))
    )
}

fn action_value_is_transient_dismissal_or_wait(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("wait" | "screenshot") => true,
        Some("keypress") => value
            .get("keys")
            .and_then(Value::as_array)
            .is_some_and(|keys| {
                keys.len() == 1
                    && keys[0].as_str().is_some_and(|key| {
                        key.eq_ignore_ascii_case("escape") || key.eq_ignore_ascii_case("esc")
                    })
            }),
        _ => false,
    }
}

fn action_commits_foreground_transition(action: &ComputerAction) -> bool {
    let ComputerAction::Hotkey { keys, .. } = action else {
        return false;
    };
    keys.iter()
        .any(|key| matches!(key.as_str(), "enter" | "return" | "escape"))
}

fn action_requires_execution(action: &Value) -> bool {
    !matches!(
        action.get("type").and_then(Value::as_str),
        Some("wait" | "screenshot")
    )
}

fn screenshot_coordinate(
    object: &Map<String, Value>,
    frame: Option<&CaptureFrameMetadata>,
) -> Result<Coordinate, Value> {
    let x = number_value(object, "x")?;
    let y = number_value(object, "y")?;
    coordinate_from_xy(
        x,
        y,
        frame.ok_or_else(|| {
            tool_error(
                "INVALID_ACTION",
                "pointer action requires a current screenshot frame",
            )
        })?,
    )
}

fn screenshot_coordinate_value(
    value: &Value,
    frame: Option<&CaptureFrameMetadata>,
) -> Result<Coordinate, Value> {
    if let Some(object) = value.as_object() {
        return screenshot_coordinate(object, frame);
    }
    let pair = value
        .as_array()
        .filter(|pair| pair.len() >= 2)
        .ok_or_else(|| {
            tool_error(
                "INVALID_ACTION",
                "drag.path points must be [x, y] pairs or {x, y} objects",
            )
        })?;
    let x = pair[0]
        .as_f64()
        .ok_or_else(|| tool_error("INVALID_ACTION", "drag.path x must be a number"))?;
    let y = pair[1]
        .as_f64()
        .ok_or_else(|| tool_error("INVALID_ACTION", "drag.path y must be a number"))?;
    coordinate_from_xy(
        x,
        y,
        frame.ok_or_else(|| {
            tool_error(
                "INVALID_ACTION",
                "pointer action requires a current screenshot frame",
            )
        })?,
    )
}

fn coordinate_from_xy(x: f64, y: f64, frame: &CaptureFrameMetadata) -> Result<Coordinate, Value> {
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= frame.width as f64
        || y >= frame.height as f64
    {
        return Err(tool_error(
            "INVALID_COORDINATE",
            format!("point ({x}, {y}) is outside the current screenshot"),
        ));
    }
    Ok(Coordinate {
        space: CoordinateSpace::ScreenshotPixel,
        point: Point { x, y },
        extent: Size {
            width: frame.width as f64,
            height: frame.height as f64,
        },
        dpi: frame.dpi,
        display_id: Some(frame.display_id.clone()),
        frame_id: Some(frame.frame_id.clone()),
    })
}

fn string_value(object: &Map<String, Value>, name: &str) -> Result<String, Value> {
    object
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| tool_error("INVALID_ACTION", format!("{name} must be a string")))
}

fn number_value(object: &Map<String, Value>, name: &str) -> Result<f64, Value> {
    object
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| tool_error("INVALID_ACTION", format!("{name} must be a number")))
}

fn integer_u32_value(value: &Value, name: &str) -> Result<u32, Value> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| tool_error("INVALID_ACTION", format!("{name} must be uint32")))
}

fn integer_i32_value(value: &Value, name: &str) -> Result<i32, Value> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| tool_error("INVALID_ACTION", format!("{name} must be int32")))
}

fn mouse_button_value(object: &Map<String, Value>, name: &str) -> Result<MouseButton, Value> {
    match object.get(name).and_then(Value::as_str).unwrap_or("left") {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        value => Err(tool_error(
            "INVALID_ACTION",
            format!("{name} must be left, right, or middle; got {value}"),
        )),
    }
}

fn key_array(object: &Map<String, Value>, name: &str) -> Result<Vec<String>, Value> {
    let Some(value) = object.get(name) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| tool_error("INVALID_ACTION", format!("{name} must be an array")))?;
    values
        .iter()
        .map(|value| {
            let key = value.as_str().ok_or_else(|| {
                tool_error(
                    "INVALID_ACTION",
                    format!("{name} must contain only strings"),
                )
            })?;
            Ok(normalize_key_name(key))
        })
        .collect()
}

fn normalize_key_name(key: &str) -> String {
    match key.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "ctrl".into(),
        "alt" | "option" => "alt".into(),
        "cmd" | "command" | "meta" | "win" | "windows" => "meta".into(),
        "arrowleft" => "left".into(),
        "arrowright" => "right".into(),
        "arrowup" => "up".into(),
        "arrowdown" => "down".into(),
        other => other.into(),
    }
}

fn mcp_execution_profile() -> ComputerApprovalProfile {
    let require_approval = env::var("ALICE_COMPUTER_MCP_REQUIRE_APPROVAL")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        });
    if require_approval {
        ComputerApprovalProfile::Conservative
    } else {
        ComputerApprovalProfile::Autonomous
    }
}

fn action_type_name(action: &ComputerAction) -> &'static str {
    match action {
        ComputerAction::Click { .. } => "click",
        ComputerAction::DoubleClick { .. } => "double_click",
        ComputerAction::RightClick { .. } => "click",
        ComputerAction::MovePointer { .. } => "move",
        ComputerAction::Drag { .. } => "drag",
        ComputerAction::Scroll { .. } => "scroll",
        ComputerAction::TypeText { .. } => "type",
        ComputerAction::KeyPress { .. } | ComputerAction::Hotkey { .. } => "keypress",
        ComputerAction::MiddleClick { .. }
        | ComputerAction::TripleClick { .. }
        | ComputerAction::ModifierClick { .. } => "click",
        ComputerAction::MouseDown { .. }
        | ComputerAction::MouseUp { .. }
        | ComputerAction::KeyDown { .. }
        | ComputerAction::KeyUp { .. }
        | ComputerAction::HoldKey { .. }
        | ComputerAction::FocusWindow { .. } => "legacy",
        ComputerAction::ModifiedPointer { action, .. } => match action {
            ComputerPointerAction::Click { clicks, .. } if *clicks == 2 => "double_click",
            ComputerPointerAction::Click { .. } => "click",
            ComputerPointerAction::Move { .. } => "move",
            ComputerPointerAction::Drag { .. } => "drag",
            ComputerPointerAction::Scroll { .. } => "scroll",
        },
    }
}

fn frame_metadata_json(
    frame: &CaptureFrameMetadata,
    screenshot: &alice_computer_use_core::Screenshot,
    cache_hit: bool,
    encode_micros: u128,
) -> Value {
    json!({
        "width": screenshot.metadata.width,
        "height": screenshot.metadata.height,
        "format": screenshot.metadata.mime_type,
        "coordinate_space": screenshot.metadata.coordinate_space,
        "frame_id": screenshot.metadata.frame_id,
        "display_id": screenshot.metadata.display_id,
        "desktop_origin": screenshot.metadata.desktop_origin,
        "dpi": screenshot.metadata.dpi,
        "scale": screenshot.metadata.scale,
        "pixel_to_desktop_scale": screenshot.metadata.pixel_to_desktop_scale,
        "captured_at": screenshot.metadata.captured_at,
        "bytes": screenshot.bytes.len(),
        "encode_cache_hit": cache_hit,
        "encode_micros": encode_micros,
        "topology_generation": frame.topology_generation,
        "pixel_format": frame.pixel_format,
        "stride": frame.stride,
        "stale_topology": frame.stale_topology,
    })
}

fn tool_error_message(error: Value) -> String {
    error
        .pointer("/structuredContent/error/message")
        .and_then(Value::as_str)
        .unwrap_or("invalid computer-use action")
        .to_owned()
}

fn computer_use_error_with_results(
    code: impl Into<String>,
    message: String,
    results: Vec<Value>,
) -> Value {
    let code = code.into();
    json!({
        "content": [{ "type": "text", "text": format!("{code}: {message}") }],
        "structuredContent": { "error": { "code": code, "message": message }, "actions": results },
        "isError": true,
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_definition(
            "computer_health",
            "Read sidecar health and adapter session status.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        tool_definition(
            "computer_observe",
            "Enumerate windows and optionally observe one semantic window.",
            json!({
                "type": "object",
                "properties": {
                    "window_id": { "type": "string", "description": "Opaque session-scoped window id." },
                    "semantic": { "type": "boolean", "default": false },
                    "capabilities": { "type": "boolean", "default": false, "description": "Read-only application capability profile for the scoped window." },
                    "screenshot_metadata": { "type": "boolean", "default": false },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64, "default": 6 },
                    "max_elements": { "type": "integer", "minimum": 1, "maximum": 4096, "default": 256 }
                },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_execute",
            "Execute one explicit semantic or pixel/input action. Local MCP defaults to autonomous execution because stdio has no approval-resume channel; set ALICE_COMPUTER_MCP_REQUIRE_APPROVAL=1 to restore approvals.",
            json!({
                "type": "object",
                "required": ["intent"],
                "properties": {
                    "intent": {
                        "type": "object",
                        "required": ["operation"],
                        "properties": {
                            "operation": { "type": "string", "enum": ["focus_window", "focus", "invoke", "set_value", "toggle", "select", "expand", "collapse", "set_range_value", "scroll_into_view", "mouse_down", "mouse_up", "middle_click", "triple_click", "modifier_click", "key_down", "key_up", "hold_key"] },
                            "element_id": { "type": "string", "description": "Opaque generation-scoped element id for semantic actions." },
                            "value": { "description": "String for set_value; number for set_range_value." },
                            "target_window_id": { "type": "string", "description": "Opaque session-scoped window id for pixel/input actions." },
                            "at": { "type": "object", "description": "Backend-neutral screen coordinate with space, point, extent, and dpi metadata." },
                            "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                            "modifier": { "type": "string", "description": "Modifier name such as ctrl or shift." },
                            "key": { "type": "string" },
                            "duration_ms": { "type": "integer", "minimum": 1, "maximum": 5000 }
                        },
                        "additionalProperties": false
                    },
                    "strategy": { "type": "string", "enum": ["semantic_only", "pixel_only", "prefer_semantic", "prefer_pixel"], "default": "prefer_semantic" },
                    "fallback_policy": { "type": "string", "enum": ["allow", "deny", "require_explicit"], "default": "deny" },
                    "execution_mode": { "type": "string", "enum": ["background_preferred", "takeover_only"], "default": "background_preferred", "description": "Prefer app/Accessibility-scoped execution without activating or moving the real pointer; takeover_only is a compatibility escape hatch." }
                },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_use",
            "Execute a bounded computer-use action batch. macOS prefers application/Accessibility-scoped background execution and renders a virtual cursor in returned screenshots; unsupported pointer paths fall back to foreground takeover. Pointer coordinates are pixels in one current screenshot frame. Pointer batches and explicit screenshot actions return a fresh image by default; set include_screenshot=false for compact output.",
            json!({
                "type": "object",
                "required": ["actions"],
                "properties": {
                    "window_id": {
                        "type": "string",
                        "description": "Optional opaque session-scoped foreground window id."
                    },
                    "include_screenshot": {
                        "type": "boolean",
                        "description": "Return a fresh PNG after the batch. When omitted, pointer batches and explicit screenshot actions return an image; keyboard/text-only batches remain compact."
                    },
                    "include_action_details": {
                        "type": "boolean",
                        "default": false,
                        "description": "Include full broker execution details for each action instead of compact outcome summaries."
                    },
                    "execution_mode": {
                        "type": "string",
                        "enum": ["background_preferred", "takeover_only"],
                        "default": "background_preferred",
                        "description": "Prefer background execution on macOS; takeover_only forces the legacy foreground path."
                    },
                    "actions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "items": {
                            "type": "object",
                            "required": ["type"],
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["click", "double_click", "scroll", "type", "wait", "keypress", "drag", "move", "screenshot"]
                                },
                                "x": { "type": "number" },
                                "y": { "type": "number" },
                                "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                                "keys": { "type": "array", "items": { "type": "string" } },
                                "text": { "type": "string" },
                                "scroll_x": { "type": "integer" },
                                "scroll_y": { "type": "integer" },
                                "duration_ms": { "type": "integer", "minimum": 0, "maximum": 10000 },
                                "path": {
                                    "type": "array",
                                    "minItems": 2,
                                    "items": {
                                        "anyOf": [
                                            {
                                                "type": "object",
                                                "required": ["x", "y"],
                                                "properties": {
                                                    "x": { "type": "number" },
                                                    "y": { "type": "number" }
                                                },
                                                "additionalProperties": false
                                            },
                                            {
                                                "type": "array",
                                                "minItems": 2,
                                                "maxItems": 2,
                                                "items": { "type": "number" }
                                            }
                                        ]
                                    }
                                }
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_screenshot",
            "Capture a PNG screenshot through the sidecar and return MCP image content.",
            json!({
                "type": "object",
                "properties": { "screen_id": { "type": "string" } },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_validate",
            "Validate a current opaque window or semantic element id without refreshing or rebinding it.",
            json!({
                "type": "object",
                "properties": {
                    "window_id": { "type": "string" },
                    "element_id": { "type": "string" }
                },
                "minProperties": 1,
                "additionalProperties": false
            }),
        ),
    ]
}

fn tool_definition(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

fn tool_success(structured: Value, text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    })
}

fn tool_error(code: impl Into<String>, message: impl Into<String>) -> Value {
    let code = code.into();
    let message = message.into();
    json!({
        "content": [{ "type": "text", "text": format!("{code}: {message}") }],
        "structuredContent": { "error": { "code": code, "message": message } },
        "isError": true,
    })
}

fn tool_broker_error(error: &BrokerError) -> Value {
    let code = error.code().to_owned();
    let message = error.to_string();
    json!({
        "content": [{ "type": "text", "text": message }],
        "structuredContent": {
            "error": {
                "code": code,
                "message": message,
                "outcome_unknown": error.outcome_unknown(),
                "retryable": error.retryable(),
            }
        },
        "isError": true,
    })
}

fn execution_tool_result(result: alice_computer_use_core::ComputerExecutionResult) -> Value {
    let is_error = matches!(
        result.final_outcome,
        ComputerExecutionOutcome::StaleElement
            | ComputerExecutionOutcome::UnknownElement
            | ComputerExecutionOutcome::ElementUnavailable
            | ComputerExecutionOutcome::Disabled
            | ComputerExecutionOutcome::ReadOnly
            | ComputerExecutionOutcome::InvalidValue
            | ComputerExecutionOutcome::WindowNotForeground
            | ComputerExecutionOutcome::FocusDenied
            | ComputerExecutionOutcome::VerificationFailed
            | ComputerExecutionOutcome::OutcomeUnknown
            | ComputerExecutionOutcome::InvalidRequest
            | ComputerExecutionOutcome::InvalidTarget
            | ComputerExecutionOutcome::BackendError
            | ComputerExecutionOutcome::IntegrityMismatch
            | ComputerExecutionOutcome::ElevationRequired
            | ComputerExecutionOutcome::UipiDenied
            | ComputerExecutionOutcome::ProtectedDesktop
            | ComputerExecutionOutcome::SecurityContextUnavailable
            | ComputerExecutionOutcome::TargetUnavailable
    );
    let structured = serde_json::to_value(&result).unwrap_or_else(|error| {
        json!({
            "final_outcome": "backend_error",
            "error": { "code": "SERIALIZATION_ERROR", "message": error.to_string() }
        })
    });
    json!({
        "content": [{ "type": "text", "text": result.explanation }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

fn json_rpc_error(code: i64, message: impl Into<String>) -> Value {
    json!({ "code": code, "message": message.into() })
}

fn sidecar_path() -> PathBuf {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--sidecar" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        }
    }
    if cfg!(windows) {
        PathBuf::from("target/release/alice-computer.exe")
    } else {
        PathBuf::from("target/release/alice-computer")
    }
}

fn run() -> AdapterResult<()> {
    let server = McpServer::new(sidecar_path());
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let count = input
            .read_line(&mut line)
            .map_err(|error| format!("MCP stdin read failed: {error}"))?;
        if count == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": json_rpc_error(-32700, format!("invalid JSON: {error}")),
                });
                serde_json::to_writer(&mut output, &response)
                    .map_err(|write_error| format!("MCP stdout write failed: {write_error}"))?;
                output
                    .write_all(b"\n")
                    .map_err(|write_error| format!("MCP stdout write failed: {write_error}"))?;
                output
                    .flush()
                    .map_err(|write_error| format!("MCP stdout flush failed: {write_error}"))?;
                continue;
            }
        };
        if let Some(response) = server.handle(request) {
            serde_json::to_writer(&mut output, &response)
                .map_err(|error| format!("MCP stdout write failed: {error}"))?;
            output
                .write_all(b"\n")
                .map_err(|error| format!("MCP stdout write failed: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("MCP stdout flush failed: {error}"))?;
        }
    }
    server.shutdown();
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{SERVER_NAME}: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_computer_use_core::{
        CoordinateSpace, DisplayId, DpiScale, FrameId, FramePixelFormat, Point,
    };
    use std::time::SystemTime;

    #[test]
    fn mcp_handshake_does_not_require_desktop_startup() {
        let server = McpServer::new(PathBuf::from(
            "/private/tmp/alice-computer-mcp-missing-sidecar-test",
        ));
        let initialize = server
            .handle(JsonRpcRequest {
                id: Some(json!(1)),
                method: "initialize".into(),
                params: None,
            })
            .expect("initialize has an id-bearing response");
        assert_eq!(initialize["result"]["serverInfo"]["name"], SERVER_NAME);

        let health = server
            .handle(JsonRpcRequest {
                id: Some(json!(2)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "computer_health",
                    "arguments": {}
                })),
            })
            .expect("health has an id-bearing response");
        assert_eq!(health["result"]["isError"], true);
        assert_eq!(
            health["result"]["structuredContent"]["error"]["code"],
            "COMPUTER_UNAVAILABLE"
        );
        assert!(server
            .session
            .lock()
            .expect("MCP session is not poisoned")
            .is_none());
    }

    #[test]
    fn wait_only_batches_do_not_start_desktop_or_require_a_window() {
        let server = McpServer::new(PathBuf::from(
            "/private/tmp/alice-computer-mcp-missing-sidecar-test",
        ));
        let result = server
            .handle(JsonRpcRequest {
                id: Some(json!(1)),
                method: "tools/call".into(),
                params: Some(json!({
                    "name": "computer_use",
                    "arguments": {
                        "include_screenshot": false,
                        "actions": [{ "type": "wait", "duration_ms": 0 }]
                    }
                })),
            })
            .expect("computer_use has an id-bearing response");
        assert_eq!(result["result"]["isError"], false);
        assert_eq!(
            result["result"]["structuredContent"]["actions"][0]["status"],
            "performed"
        );
        assert!(server
            .session
            .lock()
            .expect("MCP session is not poisoned")
            .is_none());
    }

    #[test]
    fn computer_use_tool_exposes_the_standard_action_vocabulary() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 6);
        assert!(!tools
            .iter()
            .any(|tool| tool["name"] == "computer_capabilities"));
        let computer_use = tools
            .iter()
            .find(|tool| tool["name"] == "computer_use")
            .expect("computer_use is discoverable");
        let action_types = &computer_use["inputSchema"]["properties"]["actions"]["items"]
            ["properties"]["type"]["enum"];
        for action in [
            "click",
            "double_click",
            "scroll",
            "type",
            "wait",
            "keypress",
            "drag",
            "move",
            "screenshot",
        ] {
            assert!(action_types
                .as_array()
                .expect("action type enum is an array")
                .iter()
                .any(|value| value == action));
        }
        let observe = tools
            .iter()
            .find(|tool| tool["name"] == "computer_observe")
            .expect("computer_observe remains available");
        assert_eq!(
            observe["inputSchema"]["properties"]["capabilities"]["type"],
            "boolean"
        );
    }

    fn test_frame() -> CaptureFrameMetadata {
        CaptureFrameMetadata {
            frame_id: FrameId::new("frame-1"),
            display_id: DisplayId::new("display-1"),
            topology_generation: 1,
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: Point { x: 0.0, y: 0.0 },
            width: 800,
            height: 600,
            pixel_format: FramePixelFormat::Bgra8,
            stride: 3_200,
            dpi: DpiScale::ONE,
            scale: DpiScale::ONE,
            pixel_to_desktop_scale: Some(DpiScale::ONE),
            captured_at: Some(SystemTime::UNIX_EPOCH),
            content_revision: 1,
            stale_topology: false,
        }
    }

    #[test]
    fn computer_use_coordinates_are_bound_to_the_captured_frame() {
        let frame = test_frame();
        let step = parse_computer_use_action(
            &json!({ "type": "click", "x": 12, "y": 34 }),
            Some(&frame),
            Some(WindowId::new("window-1")),
        )
        .expect("click parses");
        let ComputerUseStep::Actions(actions) = step else {
            panic!("click should become an action");
        };
        let ComputerAction::Click { at } = &actions[0] else {
            panic!("click should stay a left click");
        };
        assert_eq!(at.space, CoordinateSpace::ScreenshotPixel);
        assert_eq!(
            at.frame_id.as_ref().map(ToString::to_string),
            Some("frame-1".into())
        );
        assert_eq!(
            at.display_id.as_ref().map(ToString::to_string),
            Some("display-1".into())
        );
        assert_eq!(at.point, Point { x: 12.0, y: 34.0 });
    }

    #[test]
    fn computer_use_maps_signed_scroll_and_key_aliases() {
        let frame = test_frame();
        let step = parse_computer_use_action(
            &json!({
                "type": "scroll",
                "x": 20,
                "y": 30,
                "scroll_x": -4,
                "scroll_y": 8
            }),
            Some(&frame),
            None,
        )
        .expect("scroll parses");
        let ComputerUseStep::Actions(actions) = step else {
            panic!("scroll should become actions");
        };
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ComputerAction::ModifiedPointer {
                action: ComputerPointerAction::Scroll {
                    delta_x: -4,
                    delta_y: 8,
                    ..
                },
                modifiers,
            } if modifiers.is_empty()
        ));
        assert_eq!(normalize_key_name("CTRL"), "ctrl");
        assert_eq!(normalize_key_name("ARROWRIGHT"), "right");
    }

    #[test]
    fn global_transition_input_drops_stale_window_binding() {
        let action = ComputerAction::Hotkey {
            keys: vec!["meta".into(), "space".into()],
            target: Some(WindowId::new("old-window")),
        };
        let cleared = clear_window_target(action);
        assert!(matches!(
            cleared,
            ComputerAction::Hotkey { target: None, .. }
        ));
    }

    #[test]
    fn escape_dismissal_is_global_without_an_explicit_target() {
        let action = ComputerAction::Hotkey {
            keys: vec!["escape".into()],
            target: Some(WindowId::new("transient-menu")),
        };
        assert!(action_is_transient_dismissal(&action));
    }

    #[test]
    fn approval_profile_defaults_to_autonomous_for_stdio() {
        assert_eq!(mcp_execution_profile(), ComputerApprovalProfile::Autonomous);
    }

    #[test]
    fn computer_use_text_and_wait_do_not_require_a_screenshot_frame() {
        let text =
            parse_computer_use_action(&json!({ "type": "type", "text": "hello" }), None, None)
                .expect("text parses without a frame");
        assert!(matches!(text, ComputerUseStep::Actions(_)));
        assert!(!action_requires_frame(&json!({ "type": "wait" })));
        assert!(!action_requires_frame(&json!({ "type": "keypress" })));
        assert!(action_requires_frame(&json!({ "type": "click" })));
    }

    #[test]
    fn application_input_is_pid_scoped_but_desktop_shortcuts_stay_global() {
        let application = alice_computer_use_core::ApplicationIdentity {
            process_id: Some(42),
            executable_name: Some("Browser".into()),
            executable_path: None,
            executable_hash: None,
            process_architecture: alice_computer_use_core::ProcessArchitecture::Unknown,
            top_level_window_class: None,
            framework_hints: Vec::new(),
            version: None,
        };
        let typed = batch_execution_request(
            ComputerAction::TypeText {
                text: "hello".into(),
                target: Some(WindowId::new("old-window")),
                at: None,
            },
            Some(WindowId::new("old-window")),
            Some(application.clone()),
            false,
            ComputerExecutionMode::BackgroundPreferred,
        );
        if cfg!(target_os = "macos") {
            assert_eq!(typed.target_application(), Some(&application));
        } else {
            assert!(typed.target_application().is_none());
            assert!(matches!(
                &typed.intent,
                ComputerExecutionIntent::Pixel { target_window_id: Some(id), .. }
                    if id == &WindowId::new("old-window")
            ));
        }

        let shortcut = batch_execution_request(
            ComputerAction::Hotkey {
                keys: vec!["meta".into(), "tab".into()],
                target: Some(WindowId::new("old-window")),
            },
            Some(WindowId::new("old-window")),
            Some(application),
            false,
            ComputerExecutionMode::BackgroundPreferred,
        );
        assert!(shortcut.target_application().is_none());
        assert!(matches!(
            shortcut.intent,
            ComputerExecutionIntent::Pixel {
                target_window_id: Some(_),
                target_application: None,
                ..
            }
        ));
    }

    #[test]
    fn computer_use_preserves_multiple_modifiers_and_drag_waypoints() {
        let frame = test_frame();
        let step = parse_computer_use_action(
            &json!({
                "type": "drag",
                "button": "left",
                "keys": ["CTRL", "SHIFT"],
                "path": [
                    { "x": 1, "y": 2 },
                    { "x": 3, "y": 4 },
                    { "x": 5, "y": 6 }
                ]
            }),
            Some(&frame),
            None,
        )
        .expect("modified drag parses");
        let ComputerUseStep::Actions(actions) = step else {
            panic!("drag should become an action");
        };
        assert!(matches!(
            &actions[0],
            ComputerAction::ModifiedPointer {
                action: ComputerPointerAction::Drag { path, .. },
                modifiers,
            } if path.len() == 3
                && modifiers == &vec![String::from("ctrl"), String::from("shift")]
        ));

        let array_path = parse_computer_use_action(
            &json!({
                "type": "drag",
                "path": [[1, 2], [3, 4]]
            }),
            Some(&frame),
            None,
        )
        .expect("array drag path parses");
        let ComputerUseStep::Actions(actions) = array_path else {
            panic!("array drag should become an action");
        };
        assert!(matches!(
            &actions[0],
            ComputerAction::Drag { from, to, .. }
                if from.point == Point { x: 1.0, y: 2.0 }
                    && to.point == Point { x: 3.0, y: 4.0 }
        ));
    }

    #[test]
    fn computer_use_rejects_coordinates_outside_the_frame() {
        let error = parse_computer_use_action(
            &json!({ "type": "move", "x": 800, "y": 0 }),
            Some(&test_frame()),
            None,
        )
        .expect_err("edge coordinate must be rejected");
        assert_eq!(
            error["structuredContent"]["error"]["code"],
            "INVALID_COORDINATE"
        );
    }
}
