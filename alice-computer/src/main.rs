//! `alice-computer` is the long-lived, local Computer Use process boundary.
//!
//! The `serve` command is the production shape: one process owns one native
//! runtime and accepts framed JSON requests on stdin/stdout.  The other
//! commands are deliberately human-facing diagnostics and do not form a
//! second automation implementation.

use alice_computer_use_core::{
    ComputerAction, ComputerActionResult, ComputerError, ComputerExecutionIntent,
    ComputerExecutionRequest, ComputerExecutionResult, ComputerObservation, ComputerSessionId,
    FrameEncoding, Window, WindowId,
};
use alice_computer_use_runtime::{ComputerRuntime, WinNativeBackend};
use alice_computer_use_sidecar_protocol::{
    computer_error, decode_json, error_response, invalid_params, ok_response, read_frame,
    write_frame, ActionParams, CapabilityInfo, CapabilityProbeParams, ExecutionPerformParams,
    FrameCaptureParams, FrameCaptureResult, FrameEncodeParams, FrameEncodeResult, FrameIdParams,
    HealthResult, HelloResult, RpcError, RpcRequest, ScreenshotParams, SemanticActionParams,
    SemanticObserveParams, SemanticValidateParams, SessionCleanupResult, SessionParams,
    SessionResult, ShutdownResult, BACKEND_NAME, CAPABILITY_CONTRACT_VERSION, PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    io::{self, Write},
    path::PathBuf,
    process,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The sidecar is private to one host process and host-generated request ids
/// are monotonic for that process. Keeping a bounded replay window prevents a
/// long-lived desktop session from growing without limit while still catching
/// accidental/replayed requests in the active RPC window.
const MAX_TRACKED_REQUEST_IDS: usize = 8_192;

struct RequestReplayWindow {
    seen: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl RequestReplayWindow {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn remember(&mut self, request_id: &str) -> bool {
        if self.seen.contains(request_id) {
            return false;
        }
        let request_id = request_id.to_owned();
        self.seen.insert(request_id.clone());
        self.order.push_back(request_id);
        while self.order.len() > self.capacity {
            if let Some(expired) = self.order.pop_front() {
                self.seen.remove(&expired);
            }
        }
        true
    }
}

#[cfg(windows)]
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("alice-computer: {error}");
        process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("alice-computer requires Windows native APIs");
    process::exit(1);
}

#[cfg(windows)]
async fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => serve().await,
        Some("windows") => diagnostic_windows().await,
        Some("elements") => diagnostic_elements(args.collect()).await,
        Some("screenshot") => {
            let mut output = None;
            while let Some(argument) = args.next() {
                match argument.as_str() {
                    "--output" | "-o" => {
                        output = args.next().map(PathBuf::from);
                    }
                    "--help" | "-h" => {
                        print_screenshot_help();
                        return Ok(());
                    }
                    other => return Err(format!("unknown screenshot argument: {other}")),
                }
            }
            diagnostic_screenshot(output).await
        }
        Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}")),
    }
}

#[cfg(windows)]
fn print_help() {
    println!(
        "alice-computer\n\n  serve\n  windows\n  elements --window <WindowId> [--max-depth <n>] [--max-elements <n>]\n  screenshot --output <path>\n\nserve uses framed JSON on stdout; diagnostics use human-readable stdout."
    );
}

#[cfg(windows)]
fn print_screenshot_help() {
    println!("usage: alice-computer screenshot --output <path>");
}

#[cfg(windows)]
async fn diagnostic_windows() -> Result<(), String> {
    let runtime = ComputerRuntime::new(WinNativeBackend::new());
    runtime
        .initialize()
        .await
        .map_err(|error| error.to_string())?;
    let session = runtime
        .start_session()
        .await
        .map_err(|error| error.to_string())?;
    let desktop = runtime
        .desktop_security_context()
        .await
        .map_err(|error| error.to_string())?;
    let desktop_identity = runtime
        .native_desktop_identity()
        .await
        .map_err(|error| error.to_string())?;
    let screens = session
        .enumerate_screens()
        .await
        .map_err(|error| error.to_string())?;
    let windows = session
        .enumerate_windows()
        .await
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "pid": process::id(),
            "backend": BACKEND_NAME,
            "desktop": desktop,
            "desktop_identity": {
                "current": desktop_identity.0,
                "input": desktop_identity.1,
            },
            "screens": screens,
            "windows": windows,
        }))
        .map_err(|error| error.to_string())?
    );
    session.close().await.map_err(|error| error.to_string())?;
    runtime
        .shutdown()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(windows)]
async fn diagnostic_elements(arguments: Vec<String>) -> Result<(), String> {
    let mut window_id = None;
    let mut max_depth = None;
    let mut max_elements = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--window" | "-w" => window_id = arguments.next().map(WindowId::new),
            "--max-depth" => {
                max_depth = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--max-depth requires a value".to_owned())?
                        .parse::<u32>()
                        .map_err(|error| format!("invalid --max-depth: {error}"))?,
                )
            }
            "--max-elements" => {
                max_elements = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--max-elements requires a value".to_owned())?
                        .parse::<u32>()
                        .map_err(|error| format!("invalid --max-elements: {error}"))?,
                )
            }
            "--help" | "-h" => {
                println!("usage: alice-computer elements --window <WindowId> [--max-depth <n>] [--max-elements <n>]");
                return Ok(());
            }
            other => return Err(format!("unknown elements argument: {other}")),
        }
    }
    let window_id = window_id.ok_or_else(|| "elements requires --window <WindowId>".to_owned())?;
    let runtime = ComputerRuntime::new(WinNativeBackend::new());
    runtime
        .initialize()
        .await
        .map_err(|error| error.to_string())?;
    let session = runtime
        .start_session()
        .await
        .map_err(|error| error.to_string())?;
    let observation = session
        .semantic_observe(&window_id, {
            let defaults = alice_computer_use_core::SemanticObservationLimits::default();
            alice_computer_use_core::SemanticObservationLimits {
                max_depth: max_depth.unwrap_or(defaults.max_depth),
                max_elements: max_elements.unwrap_or(defaults.max_elements),
            }
        })
        .await
        .map_err(|error| error.to_string())?;
    println!(
        "semantic_observation window={} root={} elements={} truncated={} total_us={} uia_init_us={} tree_walk_us={} property_read_us={}",
        observation.window_id,
        observation.root_element_id,
        observation.elements.len(),
        observation.metadata.truncated,
        observation.metadata.total_micros,
        observation.metadata.uia_init_micros,
        observation.metadata.tree_walk_micros,
        observation.metadata.property_read_micros,
    );
    let by_id = observation
        .elements
        .iter()
        .map(|element| (element.id.clone(), element))
        .collect::<HashMap<_, _>>();
    print_element_tree(&by_id, &observation.root_element_id, 0);
    session.close().await.map_err(|error| error.to_string())?;
    runtime
        .shutdown()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(windows)]
fn print_element_tree(
    elements: &HashMap<
        alice_computer_use_core::ElementId,
        &alice_computer_use_core::ComputerElement,
    >,
    id: &alice_computer_use_core::ElementId,
    depth: usize,
) {
    let Some(element) = elements.get(id) else {
        return;
    };
    println!(
        "{}[{}] {} role={:?} name={:?} enabled={} focused={} focusable={} offscreen={} bounds={:?} capabilities={:?}",
        "  ".repeat(depth),
        element.id,
        element.control_type,
        element.role,
        element.name,
        element.enabled,
        element.focused,
        element.focusable,
        element.offscreen,
        element.bounds,
        element.capabilities,
    );
    for child_id in &element.child_ids {
        print_element_tree(elements, child_id, depth + 1);
    }
}

#[cfg(windows)]
async fn diagnostic_screenshot(output: Option<PathBuf>) -> Result<(), String> {
    let output = output.ok_or_else(|| "screenshot requires --output <path>".to_owned())?;
    let runtime = ComputerRuntime::new(WinNativeBackend::new());
    runtime
        .initialize()
        .await
        .map_err(|error| error.to_string())?;
    let session = runtime
        .start_session()
        .await
        .map_err(|error| error.to_string())?;
    let screen = session
        .enumerate_screens()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|screen| screen.primary)
        .ok_or_else(|| "no primary screen".to_owned())?;
    let screenshot = session
        .screenshot(&screen.id)
        .await
        .map_err(|error| error.to_string())?;
    std::fs::write(&output, &screenshot.bytes).map_err(|error| error.to_string())?;
    println!(
        "backend={BACKEND_NAME} pid={} output={} width={} height={} dpi=({}, {}) bytes={}",
        process::id(),
        output.display(),
        screenshot.metadata.width,
        screenshot.metadata.height,
        screenshot.metadata.dpi.x,
        screenshot.metadata.dpi.y,
        screenshot.bytes.len()
    );
    session.close().await.map_err(|error| error.to_string())?;
    runtime
        .shutdown()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(windows)]
struct ServerState {
    runtime: ComputerRuntime<WinNativeBackend>,
    sessions:
        HashMap<ComputerSessionId, alice_computer_use_runtime::ComputerSession<WinNativeBackend>>,
    window_maps: HashMap<ComputerSessionId, WindowMap>,
    request_ids: RequestReplayWindow,
}

#[cfg(windows)]
struct WindowMap {
    next_id: u64,
    public_to_backend: HashMap<WindowId, WindowId>,
    backend_to_public: HashMap<String, WindowId>,
}

#[cfg(windows)]
impl WindowMap {
    fn new() -> Self {
        Self {
            next_id: 1,
            public_to_backend: HashMap::new(),
            backend_to_public: HashMap::new(),
        }
    }

    fn public_id(&mut self, backend_id: &WindowId) -> WindowId {
        if let Some(public_id) = self.backend_to_public.get(backend_id.as_str()) {
            return public_id.clone();
        }
        let public_id = WindowId::new(format!("sidecar-window-{}", self.next_id));
        self.next_id = self.next_id.saturating_add(1);
        self.backend_to_public
            .insert(backend_id.as_str().to_owned(), public_id.clone());
        self.public_to_backend
            .insert(public_id.clone(), backend_id.clone());
        public_id
    }

    fn backend_id(&self, public_id: &WindowId) -> Result<WindowId, RpcError> {
        self.public_to_backend
            .get(public_id)
            .cloned()
            .ok_or_else(|| {
                RpcError::new("INVALID_WINDOW", "window id is not valid for this session")
            })
    }
}

#[cfg(windows)]
async fn serve() -> Result<(), String> {
    let runtime = ComputerRuntime::new(WinNativeBackend::new());
    runtime
        .initialize()
        .await
        .map_err(|error| error.to_string())?;
    let mut state = ServerState {
        runtime,
        sessions: HashMap::new(),
        window_maps: HashMap::new(),
        request_ids: RequestReplayWindow::new(MAX_TRACKED_REQUEST_IDS),
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    loop {
        let payload = match read_frame(&mut input) {
            Ok(Some(payload)) => payload,
            Ok(None) => break,
            Err(error) => {
                let response = error_response(
                    "unknown",
                    RpcError::new("MALFORMED_FRAME", error.to_string()),
                );
                let _ = write_frame(&mut output, &response);
                if matches!(
                    error,
                    alice_computer_use_sidecar_protocol::FrameError::Truncated
                        | alice_computer_use_sidecar_protocol::FrameError::Io(_)
                ) {
                    break;
                }
                continue;
            }
        };

        let request = match decode_json::<RpcRequest>(&payload) {
            Ok(request) => request,
            Err(error) => {
                let response = error_response(
                    "unknown",
                    RpcError::new("MALFORMED_REQUEST", error.to_string()),
                );
                write_frame(&mut output, &response)
                    .map_err(|write_error| write_error.to_string())?;
                continue;
            }
        };
        if request.request_id.is_empty() || request.request_id.len() > 128 {
            let response = error_response(
                request.request_id,
                RpcError::new("INVALID_REQUEST", "request_id must be 1..=128 bytes"),
            );
            write_frame(&mut output, &response).map_err(|write_error| write_error.to_string())?;
            continue;
        }
        if !state.request_ids.remember(&request.request_id) {
            let response = error_response(
                request.request_id,
                RpcError::new("DUPLICATE_REQUEST_ID", "request_id was already handled"),
            );
            write_frame(&mut output, &response).map_err(|write_error| write_error.to_string())?;
            continue;
        }

        let should_stop = handle_request(&mut state, request, &mut output).await?;
        if should_stop {
            break;
        }
    }

    for session in state.sessions.values() {
        let _ = session.close().await;
    }
    state
        .runtime
        .shutdown()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(windows)]
async fn handle_request(
    state: &mut ServerState,
    request: RpcRequest,
    output: &mut impl Write,
) -> Result<bool, String> {
    let request_id = request.request_id.clone();
    let response = match request.method.as_str() {
        "hello" => ok_response(
            request_id,
            &HelloResult {
                protocol_version: PROTOCOL_VERSION,
                capability_contract_version: CAPABILITY_CONTRACT_VERSION,
                sidecar_version: VERSION.to_owned(),
                backend: BACKEND_NAME.to_owned(),
                capabilities: state
                    .runtime
                    .capabilities()
                    .await
                    .into_iter()
                    .map(capability_info)
                    .collect(),
                pid: process::id(),
            },
        ),
        "health" => ok_response(
            request_id,
            &HealthResult {
                ready: true,
                initialized: true,
                session_count: state.sessions.len(),
                pid: process::id(),
                security: state.runtime.security_context().await.ok(),
                desktop: state.runtime.desktop_security_context().await.ok(),
            },
        ),
        "session.create" => match state.runtime.start_session().await {
            Ok(session) => {
                let id = session.id().clone();
                state.sessions.insert(id.clone(), session);
                state.window_maps.insert(id.clone(), WindowMap::new());
                ok_response(request_id, &SessionResult { session_id: id })
            }
            Err(error) => error_response(request_id, computer_error(error)),
        },
        "session.close" => match params::<SessionParams>(&request) {
            Ok(params) => match state.sessions.get(&params.session_id).cloned() {
                Some(session) => match session.close().await {
                    Ok(()) => {
                        state.sessions.remove(&params.session_id);
                        state.window_maps.remove(&params.session_id);
                        ok_response(request_id, &serde_json::json!({ "closed": true }))
                    }
                    Err(error) => error_response(request_id, computer_error(error)),
                },
                None => error_response(request_id, invalid_session(&params.session_id)),
            },
            Err(error) => error_response(request_id, error),
        },
        "session.cleanup_pressed" => match params::<SessionParams>(&request) {
            Ok(params) => match state.sessions.get(&params.session_id).cloned() {
                Some(session) => match session.cleanup_pressed().await {
                    Ok(()) => ok_response(request_id, &SessionCleanupResult { cleaned: true }),
                    Err(error) => error_response(request_id, computer_error(error)),
                },
                None => error_response(request_id, invalid_session(&params.session_id)),
            },
            Err(error) => error_response(request_id, error),
        },
        "window.list" => {
            match params::<SessionParams>(&request)
                .and_then(|params| session(state, &params.session_id))
            {
                Ok(session) => match session.enumerate_windows().await {
                    Ok(windows) => {
                        let windows = public_windows(state, &session.id().clone(), windows);
                        ok_response(request_id, &windows)
                    }
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                },
                Err(error) => error_response(request_id, error),
            }
        }
        "observe" => {
            match params::<SessionParams>(&request)
                .and_then(|params| session(state, &params.session_id))
            {
                Ok(session) => match session.observe().await {
                    Ok(observation) => {
                        let observation = public_observation(state, observation);
                        ok_response(request_id, &observation)
                    }
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                },
                Err(error) => error_response(request_id, error),
            }
        }
        "semantic.observe" => match params::<SemanticObserveParams>(&request) {
            Ok(params) => {
                let session_id = params.session_id.clone();
                let public_window_id = params.window_id.clone();
                let limits = params.limits();
                let resolved = session(state, &session_id).and_then(|session| {
                    backend_window_id(state, &session_id, &public_window_id)
                        .map(|window_id| (session, window_id))
                });
                match resolved {
                    Ok((session, backend_window_id)) => {
                        match session.semantic_observe(&backend_window_id, limits).await {
                            Ok(mut observation) => {
                                observation.window_id = public_window_id;
                                let serialization_started = std::time::Instant::now();
                                let _ = serde_json::to_vec(&observation);
                                observation.metadata.serialization_micros =
                                    Some(serialization_started.elapsed().as_micros());
                                ok_response(request_id, &observation)
                            }
                            Err(error) => error_response(request_id, sidecar_computer_error(error)),
                        }
                    }
                    Err(error) => error_response(request_id, error),
                }
            }
            Err(error) => error_response(request_id, error),
        },
        "capability.probe" => match params::<CapabilityProbeParams>(&request) {
            Ok(params) => {
                let session_id = params.session_id.clone();
                let public_window_id = params.window_id.clone();
                let resolved = session(state, &session_id).and_then(|session| {
                    backend_window_id(state, &session_id, &public_window_id)
                        .map(|window_id| (session, window_id))
                });
                match resolved {
                    Ok((session, backend_window_id)) => {
                        match session.capability_probe(&backend_window_id).await {
                            Ok(mut profile) => {
                                profile.window_id = public_window_id;
                                ok_response(request_id, &profile)
                            }
                            Err(error) => error_response(request_id, sidecar_computer_error(error)),
                        }
                    }
                    Err(error) => error_response(request_id, error),
                }
            }
            Err(error) => error_response(request_id, error),
        },
        "semantic.validate" => match params::<SemanticValidateParams>(&request) {
            Ok(params) => match session(state, &params.session_id) {
                Ok(session) => match session.validate_element(&params.element_id).await {
                    Ok(()) => ok_response(request_id, &serde_json::json!({ "valid": true })),
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                },
                Err(error) => error_response(request_id, error),
            },
            Err(error) => error_response(request_id, error),
        },
        "semantic.element_window" => match params::<SemanticValidateParams>(&request) {
            Ok(params) => match session(state, &params.session_id) {
                Ok(session) => match session.resolve_element_window(&params.element_id).await {
                    Ok(backend_window_id) => {
                        let public_window_id = state
                            .window_maps
                            .entry(params.session_id)
                            .or_insert_with(WindowMap::new)
                            .public_id(&backend_window_id);
                        ok_response(request_id, &public_window_id)
                    }
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                },
                Err(error) => error_response(request_id, error),
            },
            Err(error) => error_response(request_id, error),
        },
        "semantic.action" => match params::<SemanticActionParams>(&request) {
            Ok(params) => match session(state, &params.session_id) {
                Ok(session) => match session.semantic_action(&params.action).await {
                    Ok(result) => ok_response(request_id, &result),
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                },
                Err(error) => error_response(request_id, error),
            },
            Err(error) => error_response(request_id, error),
        },
        "execution.perform" => match params::<ExecutionPerformParams>(&request) {
            Ok(params) => {
                let session_id = params.session_id.clone();
                let resolved = session(state, &session_id).and_then(|session| {
                    internal_execution_request(state, &session_id, &params.execution_request)
                        .map(|request| (session, request))
                });
                match resolved {
                    Ok((session, execution_request)) => {
                        match session.execute_policy(&execution_request).await {
                            Ok(result) => {
                                let result = public_execution_result(state, &session_id, result);
                                ok_response(request_id, &result)
                            }
                            Err(error) => error_response(request_id, sidecar_computer_error(error)),
                        }
                    }
                    Err(error) => error_response(request_id, error),
                }
            }
            Err(error) => error_response(request_id, error),
        },
        "frame.capture" => match params::<FrameCaptureParams>(&request).and_then(|params| {
            session(state, &params.session_id).map(|session| (session, params.display_id))
        }) {
            Ok((session, requested_display)) => {
                let display = match requested_display {
                    Some(display) => Ok(display),
                    None => session
                        .enumerate_screens()
                        .await
                        .map_err(sidecar_computer_error)
                        .and_then(|screens| {
                            screens
                                .into_iter()
                                .find(|screen| screen.primary)
                                .map(|screen| screen.id)
                                .ok_or_else(|| {
                                    RpcError::new(
                                        "NO_PRIMARY_SCREEN",
                                        "no primary screen was reported",
                                    )
                                })
                        }),
                };
                match display {
                    Ok(display) => match session.capture_frame(&display).await {
                        Ok(metadata) => ok_response(request_id, &FrameCaptureResult { metadata }),
                        Err(error) => error_response(request_id, sidecar_computer_error(error)),
                    },
                    Err(error) => error_response(request_id, error),
                }
            }
            Err(error) => error_response(request_id, error),
        },
        "frame.metadata" => match params::<FrameIdParams>(&request)
            .and_then(|params| session(state, &params.session_id).map(|session| (session, params)))
        {
            Ok((session, params)) => match session.frame_metadata(&params.frame_id).await {
                Ok(result) => ok_response(request_id, &result),
                Err(error) => error_response(request_id, sidecar_computer_error(error)),
            },
            Err(error) => error_response(request_id, error),
        },
        "frame.encode" => match params::<FrameEncodeParams>(&request)
            .and_then(|params| session(state, &params.session_id).map(|session| (session, params)))
        {
            Ok((session, params)) => {
                let started = std::time::Instant::now();
                match session
                    .encode_frame(&params.frame_id, params.encoding)
                    .await
                {
                    Ok(encoded) => {
                        let mut result = FrameEncodeResult {
                            screenshot: encoded.screenshot,
                            encoding: encoded.encoding,
                            cache_hit: encoded.cache_hit,
                            encode_micros: encoded.encode_micros.max(started.elapsed().as_micros()),
                            transport_bytes: 0,
                        };
                        result.transport_bytes = serde_json::to_vec(&result)
                            .map(|bytes| bytes.len())
                            .unwrap_or_default();
                        ok_response(request_id, &result)
                    }
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                }
            }
            Err(error) => error_response(request_id, error),
        },
        "frame.release" => match params::<FrameIdParams>(&request)
            .and_then(|params| session(state, &params.session_id).map(|session| (session, params)))
        {
            Ok((session, params)) => match session.release_frame(&params.frame_id).await {
                Ok(result) => ok_response(request_id, &result),
                Err(error) => error_response(request_id, sidecar_computer_error(error)),
            },
            Err(error) => error_response(request_id, error),
        },
        "screenshot" => {
            match params::<ScreenshotParams>(&request).and_then(|params| {
                session(state, &params.session_id).map(|session| (session, params.screen_id))
            }) {
                Ok((session, requested_screen)) => {
                    let screen = match requested_screen {
                        Some(screen) => Ok(screen),
                        None => session
                            .enumerate_screens()
                            .await
                            .map_err(sidecar_computer_error)
                            .and_then(|screens| {
                                screens
                                    .into_iter()
                                    .find(|screen| screen.primary)
                                    .map(|screen| screen.id)
                                    .ok_or_else(|| {
                                        RpcError::new(
                                            "NO_PRIMARY_SCREEN",
                                            "no primary screen was reported",
                                        )
                                    })
                            }),
                    };
                    match screen {
                        Ok(screen) => match session.capture_frame(&screen).await {
                            Ok(frame) => match session
                                .encode_frame(&frame.frame_id, FrameEncoding::Png)
                                .await
                            {
                                Ok(encoded) => ok_response(request_id, &encoded.screenshot),
                                Err(error) => {
                                    error_response(request_id, sidecar_computer_error(error))
                                }
                            },
                            Err(error) => error_response(request_id, sidecar_computer_error(error)),
                        },
                        Err(error) => error_response(request_id, error),
                    }
                }
                Err(error) => error_response(request_id, error),
            }
        }
        "action" => {
            match params::<ActionParams>(&request).and_then(|params| {
                let session_id = params.session_id.clone();
                session(state, &session_id).and_then(|session| {
                    internal_action(state, &session_id, &params.action)
                        .map(|action| (session, action))
                })
            }) {
                Ok((session, action)) => match session.execute(&action).await {
                    Ok(result) => ok_response(request_id, &public_action_result(result)),
                    Err(error) => error_response(request_id, sidecar_computer_error(error)),
                },
                Err(error) => error_response(request_id, error),
            }
        }
        "shutdown" => {
            let sessions = state.sessions.values().cloned().collect::<Vec<_>>();
            let closed_sessions = sessions.len();
            for session in sessions {
                let _ = session.close().await;
            }
            state.sessions.clear();
            state.window_maps.clear();
            let result = state.runtime.shutdown().await;
            match result {
                Ok(()) => ok_response(
                    request_id,
                    &ShutdownResult {
                        closed_sessions,
                        stopped: true,
                    },
                ),
                Err(error) => error_response(request_id, sidecar_computer_error(error)),
            }
        }
        _ => error_response(
            request_id,
            RpcError::new(
                "UNKNOWN_METHOD",
                format!("unknown method: {}", request.method),
            ),
        ),
    };
    write_frame(output, &response).map_err(|error| error.to_string())?;
    Ok(request.method == "shutdown")
}

#[cfg(windows)]
fn public_windows(
    state: &mut ServerState,
    session_id: &ComputerSessionId,
    windows: Vec<Window>,
) -> Vec<Window> {
    let map = state
        .window_maps
        .entry(session_id.clone())
        .or_insert_with(WindowMap::new);
    windows
        .into_iter()
        .map(|mut window| {
            window.id = map.public_id(&window.id);
            window
        })
        .collect()
}

#[cfg(windows)]
fn public_observation(
    state: &mut ServerState,
    mut observation: ComputerObservation,
) -> ComputerObservation {
    let session_id = observation.session_id.clone();
    let active_backend_id = observation.active_window.clone();
    observation.windows = public_windows(state, &session_id, observation.windows);
    observation.active_window = active_backend_id.and_then(|backend_id| {
        state
            .window_maps
            .get(&session_id)
            .and_then(|map| map.backend_to_public.get(backend_id.as_str()).cloned())
    });
    observation
}

#[cfg(windows)]
fn backend_window_id(
    state: &ServerState,
    session_id: &ComputerSessionId,
    public_window_id: &WindowId,
) -> Result<WindowId, RpcError> {
    state
        .window_maps
        .get(session_id)
        .ok_or_else(|| invalid_session(session_id))?
        .backend_id(public_window_id)
}

#[cfg(windows)]
fn internal_action(
    state: &ServerState,
    session_id: &ComputerSessionId,
    action: &ComputerAction,
) -> Result<ComputerAction, RpcError> {
    let map = state
        .window_maps
        .get(session_id)
        .ok_or_else(|| invalid_session(session_id))?;
    let target = |target: &Option<WindowId>| {
        target
            .as_ref()
            .map(|window_id| map.backend_id(window_id))
            .transpose()
    };
    match action {
        ComputerAction::FocusWindow { window_id } => Ok(ComputerAction::FocusWindow {
            window_id: map.backend_id(window_id)?,
        }),
        ComputerAction::TypeText {
            text,
            target: window,
            at,
        } => Ok(ComputerAction::TypeText {
            text: text.clone(),
            target: target(window)?,
            at: at.clone(),
        }),
        ComputerAction::KeyPress {
            key,
            target: window,
        } => Ok(ComputerAction::KeyPress {
            key: key.clone(),
            target: target(window)?,
        }),
        ComputerAction::Hotkey {
            keys,
            target: window,
        } => Ok(ComputerAction::Hotkey {
            keys: keys.clone(),
            target: target(window)?,
        }),
        ComputerAction::MouseDown {
            button,
            at,
            target: window,
        } => Ok(ComputerAction::MouseDown {
            button: *button,
            at: at.clone(),
            target: target(window)?,
        }),
        ComputerAction::MouseUp {
            button,
            at,
            target: window,
        } => Ok(ComputerAction::MouseUp {
            button: *button,
            at: at.clone(),
            target: target(window)?,
        }),
        ComputerAction::MiddleClick { at, target: window } => Ok(ComputerAction::MiddleClick {
            at: at.clone(),
            target: target(window)?,
        }),
        ComputerAction::TripleClick { at, target: window } => Ok(ComputerAction::TripleClick {
            at: at.clone(),
            target: target(window)?,
        }),
        ComputerAction::ModifierClick {
            modifier,
            button,
            at,
            target: window,
        } => Ok(ComputerAction::ModifierClick {
            modifier: modifier.clone(),
            button: *button,
            at: at.clone(),
            target: target(window)?,
        }),
        ComputerAction::KeyDown {
            key,
            target: window,
        } => Ok(ComputerAction::KeyDown {
            key: key.clone(),
            target: target(window)?,
        }),
        ComputerAction::KeyUp {
            key,
            target: window,
        } => Ok(ComputerAction::KeyUp {
            key: key.clone(),
            target: target(window)?,
        }),
        ComputerAction::HoldKey {
            key,
            duration_ms,
            target: window,
        } => Ok(ComputerAction::HoldKey {
            key: key.clone(),
            duration_ms: *duration_ms,
            target: target(window)?,
        }),
        other => Ok(other.clone()),
    }
}

#[cfg(windows)]
fn internal_execution_request(
    state: &ServerState,
    session_id: &ComputerSessionId,
    request: &ComputerExecutionRequest,
) -> Result<ComputerExecutionRequest, RpcError> {
    let map = state
        .window_maps
        .get(session_id)
        .ok_or_else(|| invalid_session(session_id))?;
    let mut internal = request.clone();
    if let ComputerExecutionIntent::Pixel {
        action,
        target_window_id,
    } = &mut internal.intent
    {
        *action = internal_action(state, session_id, action)?;
        *target_window_id = target_window_id
            .as_ref()
            .map(|window_id| map.backend_id(window_id))
            .transpose()?;
    }
    Ok(internal)
}

#[cfg(windows)]
fn public_execution_result(
    state: &mut ServerState,
    session_id: &ComputerSessionId,
    mut result: ComputerExecutionResult,
) -> ComputerExecutionResult {
    let map = state
        .window_maps
        .entry(session_id.clone())
        .or_insert_with(WindowMap::new);
    let public_window = |map: &mut WindowMap, window_id: &WindowId| map.public_id(window_id);
    match &mut result.requested_intent {
        ComputerExecutionIntent::Pixel {
            action,
            target_window_id,
        } => {
            *action = public_action(action.clone(), map);
            *target_window_id = target_window_id
                .as_ref()
                .map(|window_id| public_window(map, window_id));
        }
        ComputerExecutionIntent::Semantic(_) => {}
    }
    if let Some(target) = &mut result.pixel_target {
        target.window_id = public_window(map, &target.window_id);
    }
    for attempt in &mut result.attempts {
        if let Some(target) = &mut attempt.pixel_target {
            target.window_id = public_window(map, &target.window_id);
        }
    }
    result
}

#[cfg(windows)]
fn public_action(mut action: ComputerAction, map: &mut WindowMap) -> ComputerAction {
    let public = |map: &mut WindowMap, target: &Option<WindowId>| {
        target.as_ref().map(|window_id| map.public_id(window_id))
    };
    match &mut action {
        ComputerAction::TypeText { target, .. }
        | ComputerAction::KeyPress { target, .. }
        | ComputerAction::Hotkey { target, .. }
        | ComputerAction::MouseDown { target, .. }
        | ComputerAction::MouseUp { target, .. }
        | ComputerAction::MiddleClick { target, .. }
        | ComputerAction::TripleClick { target, .. }
        | ComputerAction::ModifierClick { target, .. }
        | ComputerAction::KeyDown { target, .. }
        | ComputerAction::KeyUp { target, .. }
        | ComputerAction::HoldKey { target, .. } => {
            *target = public(map, target);
        }
        ComputerAction::FocusWindow { window_id } => {
            *window_id = map.public_id(window_id);
        }
        ComputerAction::Click { .. }
        | ComputerAction::DoubleClick { .. }
        | ComputerAction::RightClick { .. }
        | ComputerAction::MovePointer { .. }
        | ComputerAction::Drag { .. }
        | ComputerAction::Scroll { .. } => {}
    }
    action
}

#[cfg(windows)]
fn public_action_result(mut result: ComputerActionResult) -> ComputerActionResult {
    if let Some(detail) = result.backend_detail.take() {
        result.backend_detail = Some(if detail.contains("hwnd=") {
            "backend=win-native; action=focus_window; window_id=<opaque>; foreground_confirmed=true"
                .into()
        } else {
            detail
        });
    }
    result
}

#[cfg(windows)]
fn sidecar_computer_error(error: ComputerError) -> RpcError {
    let mut rpc_error = computer_error(error);
    rpc_error.message = match rpc_error.code.as_str() {
        "INVALID_WINDOW" => "window id is invalid or no longer live".into(),
        "FOREGROUND_DENIED" => "foreground policy denied the target; input was rejected".into(),
        "FOCUS_NOT_ACQUIRED" => "target did not become the foreground window".into(),
        "ELEVATION_REQUIRED" => {
            "target requires elevation; action was rejected before any input/UIA side effect".into()
        }
        "INTEGRITY_MISMATCH" => {
            "target integrity is outside the sidecar access boundary; action was rejected".into()
        }
        "UIPI_DENIED" => "UIPI boundary denied the action before input injection".into(),
        "PROTECTED_DESKTOP" => {
            "protected or secure desktop; action was rejected without desktop switching".into()
        }
        "SECURITY_CONTEXT_UNAVAILABLE" => {
            "security context could not be confirmed; action failed closed".into()
        }
        "TARGET_UNAVAILABLE" => {
            "target window/process changed or exited; action was rejected".into()
        }
        _ => rpc_error.message,
    };
    rpc_error
}

#[cfg(windows)]
fn params<T: DeserializeOwned>(request: &RpcRequest) -> Result<T, RpcError> {
    serde_json::from_value(request.params.clone())
        .map_err(|error| invalid_params(error.to_string()))
}

#[cfg(windows)]
fn session(
    state: &ServerState,
    id: &ComputerSessionId,
) -> Result<alice_computer_use_runtime::ComputerSession<WinNativeBackend>, RpcError> {
    state
        .sessions
        .get(id)
        .cloned()
        .ok_or_else(|| invalid_session(id))
}

#[cfg(windows)]
fn invalid_session(id: &ComputerSessionId) -> RpcError {
    RpcError::new("INVALID_SESSION", format!("session is not valid: {id}"))
}

#[cfg(windows)]
fn capability_info(capability: alice_computer_use_runtime::Capability) -> CapabilityInfo {
    let state = match capability.state {
        alice_computer_use_runtime::CapabilityState::Supported => "supported",
        alice_computer_use_runtime::CapabilityState::Limited => "limited",
        alice_computer_use_runtime::CapabilityState::Unsupported => "unsupported",
    };
    CapabilityInfo {
        name: capability.name.to_owned(),
        state: state.to_owned(),
        detail: capability.detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::RequestReplayWindow;

    #[test]
    fn request_replay_window_rejects_recent_duplicates_and_stays_bounded() {
        let mut window = RequestReplayWindow::new(2);
        assert!(window.remember("r1"));
        assert!(!window.remember("r1"));
        assert!(window.remember("r2"));
        assert!(window.remember("r3"));
        assert_eq!(window.seen.len(), 2);
        assert_eq!(window.order.len(), 2);
        assert!(window.remember("r1"));
    }
}
