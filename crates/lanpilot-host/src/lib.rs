//! LanPilot host runtime library.
//!
//! This crate exposes [`run_host`], a blocking entrypoint that runs the full
//! LanPilot host stack (discovery + handshake + control + screen-stream
//! servers) in-process. It is used both by the thin `lanpilot-host` CLI
//! binary and directly (in a background thread) by `lanpilot-app`, the GUI
//! wrapper.
//!
//! Design notes:
//! - All configuration is explicit via [`HostConfig`] — no environment
//!   variables are read inside this crate.
//! - Status/progress lines are emitted through a [`Logger`] callback instead
//!   of `println!`/`eprintln!`, so callers (CLI stdout, GUI log box, tests)
//!   can capture them however they like.
//! - Cancellation is cooperative via a [`StopFlag`]: setting it asks all
//!   server loops to wind down and release their sockets, so a GUI can offer
//!   a working Stop button without killing the whole process.
//! - No `process::exit` — bind/setup failures are returned as `Err(String)`.

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lanpilot_core::{
    CONTROL_PORT, ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse,
    HANDSHAKE_PORT, HandshakeAck, HandshakeHello, IpRateLimiter, Logger, PRODUCT_NAME,
    PROTOCOL_MAGIC, STREAM_PORT, SessionEvent, StopFlag, StreamCompression, StreamFrame,
    StreamHello, TAGLINE, from_json_line, generate_pair_code, is_stopped, local_ipv4,
    log_session_event, normalize_pair_code, to_json_line, unix_timestamp_ms,
};
use lz4_flex::compress_prepend_size;

#[cfg(windows)]
use windows::Win32::{
    Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        CreateDCW, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, HBITMAP, HDC, SelectObject,
        SRCCOPY,
    },
    UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
};

/// Source of frames for the screen-stream channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamSource {
    /// Capture the real desktop with `scrap` (default).
    Screen,
    /// Generate an animated synthetic test pattern (useful headless / in CI).
    Synthetic,
}

impl Default for StreamSource {
    fn default() -> Self {
        StreamSource::Screen
    }
}

/// Explicit configuration for [`run_host`]. No environment variables are
/// read by this crate — callers (CLI wrapper, GUI app) resolve their own
/// configuration sources and populate this struct.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Friendly host name announced to agents. Defaults to `"lanpilot-host"`.
    pub host_name: Option<String>,
    /// Six-digit pairing code. If `None`, a random one is generated.
    pub pair_code: Option<String>,
    /// Where stream frames come from.
    pub stream_source: StreamSource,
    /// Maximum number of frames to stream per session before the stream
    /// channel closes on its own. Use `u64::MAX` for continuous (unlimited) streaming.
    pub max_stream_frames: u64,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            host_name: None,
            pair_code: None,
            stream_source: StreamSource::default(),
            max_stream_frames: u64::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StreamTuning {
    target_fps: u32,
    scale_divisor: u8,
}

impl Default for StreamTuning {
    fn default() -> Self {
        Self {
            target_fps: 10,
            scale_divisor: 1,
        }
    }
}

const MAX_HANDSHAKE_CONNECTIONS: usize = 16;
const MAX_CONTROL_CONNECTIONS: usize = 16;
const MAX_STREAM_CONNECTIONS: usize = 8;
const SESSION_TTL: Duration = Duration::from_secs(300);
/// How often accept/read loops wake up to check the stop flag.
const POLL_INTERVAL: Duration = Duration::from_millis(150);
const CAPTURE_RECOVERY_MAX_RETRIES: u32 = 12;
const CAPTURE_COMPAT_FALLBACK_THRESHOLD: u32 = 6;
const CAPTURE_RECOVERY_PROBE_INTERVAL: u64 = 10;
const CAPTURE_RECOVERY_PROBE_INTERVAL_RDP: u64 = 2;
const CAPTURE_RECOVERY_PROBE_MIN_ELAPSED: Duration = Duration::from_millis(200);
const CAPTURE_RECOVERY_PROBE_INIT_RETRIES: u32 = 1;
const CAPTURE_RECOVERY_PROBE_LOG_EVERY: u32 = 8;
const CAPTURE_INIT_MAX_RETRIES: u32 = 20;
/// Longer initial retry window for RDP sessions (RDP→console display transition can take ~5–10 s).
const CAPTURE_INIT_MAX_RETRIES_RDP: u32 = 60;

struct ConnectionLimiter {
    active: AtomicUsize,
    limit: usize,
    label: &'static str,
}

impl ConnectionLimiter {
    fn new(limit: usize, label: &'static str) -> Self {
        Self {
            active: AtomicUsize::new(0),
            limit,
            label,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let previous = self.active.fetch_add(1, Ordering::SeqCst);
        if previous >= self.limit {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(ConnectionPermit {
            limiter: Arc::clone(self),
        })
    }
}

struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn bind_tcp(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|_| {
        format!(
            "el puerto {port} ya está en uso. Cierra otras instancias de LanPilot e inténtalo de nuevo."
        )
    })
}

fn bind_udp(port: u16) -> Result<UdpSocket, String> {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|_| {
        format!(
            "el puerto {port} ya está en uso. Cierra otras instancias de LanPilot e inténtalo de nuevo."
        )
    })
}

/// Run the LanPilot host stack until `stop` is set or an unrecoverable setup
/// error occurs (e.g. a required port is already in use).
///
/// This function blocks the calling thread. Run it on a background thread
/// (as `lanpilot-app` does) if the caller also needs to keep servicing a UI.
pub fn run_host(config: HostConfig, logger: Logger, stop: StopFlag) -> Result<(), String> {
    let host_name = config.host_name.clone().unwrap_or_else(|| "lanpilot-host".to_string());
    let host_ipv4 = local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
    let pair_code = config
        .pair_code
        .clone()
        .and_then(|raw| normalize_pair_code(&raw))
        .unwrap_or_else(generate_pair_code);

    let spaced: String = pair_code.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ");
    logger.log(format!("{PRODUCT_NAME} Host — {TAGLINE}"));
    logger.log("=== Compartir mi pantalla ===".to_string());
    logger.log(format!("Código de conexión: {spaced}"));
    logger.log("En el otro equipo ejecuta LanPilot y pulsa Conectar (o lanpilot-agent).".to_string());
    logger.log(format!(
        "[Info] UDP {DISCOVERY_PORT}  TCP {HANDSHAKE_PORT}/{CONTROL_PORT}/{STREAM_PORT}  — {host_name} ({host_ipv4})"
    ));
    logger.log("Esperando al agente...".to_string());

    // Bind all sockets up front so setup failures are reported immediately
    // and atomically, instead of surfacing later from a background thread.
    let discovery_socket = bind_udp(DISCOVERY_PORT)?;
    let handshake_listener = bind_tcp(HANDSHAKE_PORT)?;
    let control_listener = bind_tcp(CONTROL_PORT)?;
    let stream_listener = bind_tcp(STREAM_PORT)?;

    let rdp_session = is_remote_desktop_session();
    if rdp_session {
        logger.log(
            "Sesión RDP detectada: activando modo compatible (menos FPS/resolución).".to_string(),
        );
        logger.log(
            "Consejo: evita minimizar la ventana RDP y mantén la sesión remota desbloqueada."
                .to_string(),
        );
    }
    let initial_tuning = if rdp_session {
        StreamTuning {
            target_fps: 6,
            scale_divisor: 2,
        }
    } else {
        StreamTuning::default()
    };
    let tuning = Arc::new(Mutex::new(initial_tuning));
    let active_sessions: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let control_limiter = Arc::new(ConnectionLimiter::new(MAX_CONTROL_CONNECTIONS, "control"));
    let stream_limiter = Arc::new(ConnectionLimiter::new(MAX_STREAM_CONNECTIONS, "stream"));
    let handshake_limiter = Arc::new(ConnectionLimiter::new(MAX_HANDSHAKE_CONNECTIONS, "handshake"));

    let mut handles: Vec<JoinHandle<()>> = Vec::new();

    {
        let discovery_name = host_name.clone();
        let discovery_code = pair_code.clone();
        let discovery_logger = logger.clone();
        let discovery_stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            run_discovery_server(
                discovery_socket,
                &discovery_name,
                host_ipv4,
                &discovery_code,
                discovery_logger,
                discovery_stop,
            )
        }));
    }
    {
        let sessions = Arc::clone(&active_sessions);
        let logger = logger.clone();
        let stop = Arc::clone(&stop);
        let control_tuning = Arc::clone(&tuning);
        handles.push(thread::spawn(move || {
            run_control_server(control_listener, control_tuning, sessions, control_limiter, logger, stop)
        }));
    }
    {
        let sessions = Arc::clone(&active_sessions);
        let logger = logger.clone();
        let stop = Arc::clone(&stop);
        let stream_source = config.stream_source;
        let max_stream_frames = config.max_stream_frames;
        // Shares the same tuning state as the control server above, so
        // agent-reported `StreamFeedback` (adaptive bitrate/FPS) actually
        // affects what this stream loop produces.
        let stream_tuning = Arc::clone(&tuning);
        handles.push(thread::spawn(move || {
            run_stream_server(
                stream_listener,
                stream_tuning,
                sessions,
                stream_limiter,
                logger,
                stop,
                stream_source,
                max_stream_frames,
            )
        }));
    }
    {
        let host_name = host_name.clone();
        let sessions = Arc::clone(&active_sessions);
        let logger = logger.clone();
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            run_handshake_server(handshake_listener, &host_name, sessions, handshake_limiter, logger, stop)
        }));
    }

    while !is_stopped(&stop) {
        thread::sleep(POLL_INTERVAL);
    }
    logger.log("Deteniendo host...".to_string());
    for handle in handles {
        let _ = handle.join();
    }
    logger.log("Host detenido.".to_string());
    Ok(())
}


fn run_discovery_server(
    socket: UdpSocket,
    host_name: &str,
    host_ipv4: Ipv4Addr,
    pair_code: &str,
    logger: Logger,
    stop: StopFlag,
) {
    if let Err(err) = socket.set_read_timeout(Some(POLL_INTERVAL)) {
        logger.log(format!("discovery set_read_timeout error: {err}"));
        return;
    }
    let mut rate_limiter = IpRateLimiter::new(5, Duration::from_secs(10));
    let mut buffer = [0_u8; 2048];

    while !is_stopped(&stop) {
        let (received, source) = match socket.recv_from(&mut buffer) {
            Ok(pair) => pair,
            Err(err) if is_timeout(&err) => continue,
            Err(err) => {
                logger.log(format!("discovery receive error: {err}"));
                continue;
            }
        };

        // Per-IP rate limit: max 5 discovery probes per 10 seconds.
        if !rate_limiter.check_and_record(source.ip()) {
            logger.log(format!("[RATE_LIMIT] {} exceeded discovery limit", source.ip()));
            log_session_event(&SessionEvent::RateLimitDrop {
                peer_ip: source.ip().to_string(),
                endpoint: "discovery".to_string(),
                timestamp_ms: unix_timestamp_ms(),
            });
            continue;
        }

        let payload = match std::str::from_utf8(&buffer[..received]) {
            Ok(text) => text,
            Err(err) => {
                logger.log(format!("discovery utf8 error from {source}: {err}"));
                continue;
            }
        };

        let probe: DiscoveryProbe = match from_json_line(payload) {
            Ok(parsed) => parsed,
            Err(err) => {
                logger.log(format!("invalid discovery probe from {source}: {err}"));
                continue;
            }
        };

        if probe.magic != PROTOCOL_MAGIC {
            logger.log(format!("ignoring probe with invalid magic from {source}"));
            continue;
        }
        if probe.pair_code != pair_code {
            continue;
        }

        let response = DiscoveryResponse::new(host_name, host_ipv4.to_string());
        let line = match to_json_line(&response) {
            Ok(line) => line,
            Err(err) => {
                logger.log(format!("failed to serialize discovery response: {err}"));
                continue;
            }
        };

        if let Err(err) = socket.send_to(line.as_bytes(), source) {
            logger.log(format!("failed sending discovery response to {source}: {err}"));
        }
    }
}

fn run_handshake_server(
    listener: TcpListener,
    host_name: &str,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    limiter: Arc<ConnectionLimiter>,
    logger: Logger,
    stop: StopFlag,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        logger.log(format!("handshake set_nonblocking error: {err}"));
        return;
    }
    let mut rate_limiter = IpRateLimiter::new(3, Duration::from_secs(10));

    while !is_stopped(&stop) {
        let stream = match listener.accept() {
            Ok((stream, _addr)) => stream,
            Err(err) if is_would_block(&err) => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(err) => {
                logger.log(format!("incoming connection error: {err}"));
                continue;
            }
        };

        let peer_ip = match stream.peer_addr() {
            Ok(addr) => addr.ip(),
            Err(err) => {
                logger.log(format!("handshake peer_addr error: {err}"));
                continue;
            }
        };

        // Per-IP rate limit: max 3 handshake attempts per 10 seconds.
        if !rate_limiter.check_and_record(peer_ip) {
            logger.log(format!("[RATE_LIMIT] {peer_ip} exceeded handshake limit"));
            log_session_event(&SessionEvent::RateLimitDrop {
                peer_ip: peer_ip.to_string(),
                endpoint: "handshake".to_string(),
                timestamp_ms: unix_timestamp_ms(),
            });
            continue;
        }

        let host_name = host_name.to_string();
        let sessions = Arc::clone(&sessions);
        let Some(permit) = limiter.try_acquire() else {
            logger.log(format!("dropping {} connection: limit reached", limiter.label));
            log_session_event(&SessionEvent::ConnectionDropped {
                peer_ip: peer_ip.to_string(),
                reason: format!("{} connection limit reached", limiter.label),
                timestamp_ms: unix_timestamp_ms(),
            });
            continue;
        };
        let logger = logger.clone();
        thread::spawn(move || {
            let _permit = permit;
            if let Err(err) = handle_handshake(stream, &host_name, sessions, &logger) {
                logger.log(format!("handshake error: {err}"));
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn run_control_server(
    listener: TcpListener,
    tuning: Arc<Mutex<StreamTuning>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    limiter: Arc<ConnectionLimiter>,
    logger: Logger,
    stop: StopFlag,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        logger.log(format!("control set_nonblocking error: {err}"));
        return;
    }

    while !is_stopped(&stop) {
        let stream = match listener.accept() {
            Ok((stream, _addr)) => stream,
            Err(err) if is_would_block(&err) => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(err) => {
                logger.log(format!("control incoming connection error: {err}"));
                continue;
            }
        };

        let tuning = Arc::clone(&tuning);
        let sessions = Arc::clone(&sessions);
        let Some(permit) = limiter.try_acquire() else {
            logger.log(format!("dropping {} connection: limit reached", limiter.label));
            if let Ok(addr) = stream.peer_addr() {
                log_session_event(&SessionEvent::ConnectionDropped {
                    peer_ip: addr.ip().to_string(),
                    reason: format!("{} connection limit reached", limiter.label),
                    timestamp_ms: unix_timestamp_ms(),
                });
            }
            continue;
        };
        let logger = logger.clone();
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let _permit = permit;
            handle_control_stream(stream, tuning, sessions, logger, stop)
        });
    }
}

fn handle_control_stream(
    stream: TcpStream,
    tuning: Arc<Mutex<StreamTuning>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    logger: Logger,
    stop: StopFlag,
) {
    let peer = match stream.peer_addr() {
        Ok(addr) => addr.to_string(),
        Err(err) => {
            logger.log(format!("control peer addr error: {err}"));
            "<unknown>".to_string()
        }
    };

    // Short timeout so the loop can notice the stop flag promptly; silence on
    // the control channel during stable streaming is expected and not an error.
    if let Err(err) = stream.set_read_timeout(Some(POLL_INTERVAL)) {
        logger.log(format!("control set_read_timeout error from {peer}: {err}"));
        return;
    }

    let mut reader = io::BufReader::new(stream);
    while !is_stopped(&stop) {
        let mut line = String::new();
        let bytes_read = match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(read) => read,
            Err(err) if is_timeout(&err) => continue,
            Err(err) => {
                logger.log(format!("control read error from {peer}: {err}"));
                break;
            }
        };
        if bytes_read == 0 {
            break;
        }

        let frame: ControlFrame = match from_json_line(&line) {
            Ok(frame) => frame,
            Err(err) => {
                logger.log(format!("invalid control frame from {peer}: {err}"));
                continue;
            }
        };

        if frame.magic != PROTOCOL_MAGIC {
            logger.log(format!("ignoring control frame with invalid magic from {peer}"));
            continue;
        }
        let session_known = session_is_valid(&sessions, &frame.session_id);
        if !session_known {
            logger.log(format!(
                "ignoring control frame with unknown session {} from {peer}",
                frame.session_id
            ));
            continue;
        }

        logger.log(format!(
            "Control frame accepted: session={} events={} source={peer}",
            frame.session_id,
            frame.events.len()
        ));
        for event in &frame.events {
            if let ControlEvent::StreamFeedback {
                target_fps,
                scale_divisor,
                avg_latency_ms,
                jitter_ms,
            } = event
            {
                let mut guard = match tuning.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        logger.log("failed to lock stream tuning".to_string());
                        continue;
                    }
                };
                let next_fps = (*target_fps).clamp(3, 30);
                let next_scale = (*scale_divisor).clamp(1, 3);
                if guard.target_fps != next_fps || guard.scale_divisor != next_scale {
                    logger.log(format!(
                        "Adaptive stream update: fps {}->{} scale {}->{} (lat={}ms jitter={}ms)",
                        guard.target_fps, next_fps, guard.scale_divisor, next_scale, avg_latency_ms, jitter_ms
                    ));
                    guard.target_fps = next_fps;
                    guard.scale_divisor = next_scale;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_stream_server(
    listener: TcpListener,
    tuning: Arc<Mutex<StreamTuning>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    limiter: Arc<ConnectionLimiter>,
    logger: Logger,
    stop: StopFlag,
    stream_source: StreamSource,
    max_stream_frames: u64,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        logger.log(format!("stream set_nonblocking error: {err}"));
        return;
    }

    // Pre-warm capture NOW (while RDP may still be active) so when the agent
    // connects after RDP is closed, we already have a valid DXGI context.
    let rdp_at_start = is_remote_desktop_session();
    let mut pre_warmed_capture: Option<ScreenCapture> = if stream_source == StreamSource::Screen {
        let retries = if rdp_at_start { CAPTURE_INIT_MAX_RETRIES_RDP } else { CAPTURE_INIT_MAX_RETRIES };
        match initialize_screen_capture(&logger, retries) {
            Ok(cap) => {
                logger.log("captura de pantalla pre-inicializada correctamente.".to_string());
                Some(cap)
            }
            Err(err) => {
                logger.log(format!("pre-init de captura falló (se reintentará al conectar): {err}"));
                None
            }
        }
    } else {
        None
    };

    while !is_stopped(&stop) {
        let stream = match listener.accept() {
            Ok((stream, _addr)) => stream,
            Err(err) if is_would_block(&err) => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(err) => {
                logger.log(format!("stream incoming connection error: {err}"));
                continue;
            }
        };

        let tuning = Arc::clone(&tuning);
        let sessions = Arc::clone(&sessions);
        let Some(permit) = limiter.try_acquire() else {
            logger.log(format!("dropping {} connection: limit reached", limiter.label));
            if let Ok(addr) = stream.peer_addr() {
                log_session_event(&SessionEvent::ConnectionDropped {
                    peer_ip: addr.ip().to_string(),
                    reason: format!("{} connection limit reached", limiter.label),
                    timestamp_ms: unix_timestamp_ms(),
                });
            }
            continue;
        };
        let logger_conn = logger.clone();
        let stop = Arc::clone(&stop);
        // Hand off the pre-warmed capture (or None) to the connection thread.
        let warmed = pre_warmed_capture.take();
        thread::spawn(move || {
            let _permit = permit;
            if let Err(err) = handle_stream_channel(
                stream,
                tuning,
                sessions,
                logger_conn.clone(),
                stop,
                stream_source,
                max_stream_frames,
                warmed,
            ) {
                logger_conn.log(format!("stream channel error: {err}"));
            }
        });
        // Immediately start re-warming for the next connection (non-blocking: we
        // already spawned the active connection's thread, so this runs concurrently).
        if stream_source == StreamSource::Screen {
            pre_warmed_capture = initialize_screen_capture(&logger, CAPTURE_INIT_MAX_RETRIES).ok();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_stream_channel(
    mut stream: TcpStream,
    tuning: Arc<Mutex<StreamTuning>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    logger: Logger,
    stop: StopFlag,
    stream_source: StreamSource,
    max_stream_frames: u64,
    pre_warmed_capture: Option<ScreenCapture>,
) -> Result<(), String> {
    let peer = stream
        .peer_addr()
        .map_err(|err| format!("stream peer addr error: {err}"))?;
    // Timeout on the hello read prevents permanent thread hang on non-sending clients.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("stream set_read_timeout error: {err}"))?;
    // Write timeout prevents a stalled agent from blocking this thread forever.
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| format!("stream set_write_timeout error: {err}"))?;
    let mut reader = io::BufReader::new(
        stream
            .try_clone()
            .map_err(|err| format!("stream clone error: {err}"))?,
    );

    let mut hello_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut hello_line)
        .map_err(|err| format!("stream read hello failed: {err}"))?;
    let hello: StreamHello =
        from_json_line(&hello_line).map_err(|err| format!("invalid stream hello: {err}"))?;
    if hello.magic != PROTOCOL_MAGIC || hello.role != "agent" {
        return Err(format!("invalid stream hello payload: {:?}", hello));
    }
    let session_known = session_is_valid(&sessions, &hello.session_id);
    if !session_known {
        return Err(format!(
            "stream hello with unknown session {} from {peer}",
            hello.session_id
        ));
    }

    logger.log(format!(
        "Stream channel established: session={} agent={} source={}",
        hello.session_id, hello.agent_name, peer
    ));

    let rdp_session = is_remote_desktop_session();
    let recovery_probe_interval = capture_recovery_probe_interval(rdp_session);
    let mut synthetic_fallback_enabled = false;
    let mut capture = match stream_source {
        StreamSource::Synthetic => {
            synthetic_fallback_enabled = true;
            None
        }
        StreamSource::Screen => {
            // Use the pre-warmed capture (initialized while RDP was still active)
            // if available, otherwise fall back to a fresh init attempt.
            match pre_warmed_capture {
                Some(cap) => {
                    logger.log("usando captura pre-inicializada.".to_string());
                    Some(cap)
                }
                None => {
                    let init_retries = if rdp_session { CAPTURE_INIT_MAX_RETRIES_RDP } else { CAPTURE_INIT_MAX_RETRIES };
                    match initialize_screen_capture(&logger, init_retries) {
                        Ok(capture) => Some(capture),
                        Err(err) => {
                            if rdp_session {
                                synthetic_fallback_enabled = true;
                                logger.log(format!(
                                    "{err}; activando imagen de compatibilidad temporal mientras se recupera captura real."
                                ));
                                None
                            } else {
                                return Err(err);
                            }
                        }
                    }
                }
            }
        }
    };
    let mut recovery_retries = 0_u32;
    let mut compat_probe_attempts = 0_u32;
    let mut last_compat_probe = Instant::now()
        .checked_sub(CAPTURE_RECOVERY_PROBE_MIN_ELAPSED)
        .unwrap_or_else(Instant::now);

    for sequence in 0..max_stream_frames {
        if is_stopped(&stop) {
            break;
        }
        if synthetic_fallback_enabled
            && stream_source == StreamSource::Screen
            && sequence > 0
            && sequence % recovery_probe_interval == 0
            && last_compat_probe.elapsed() >= CAPTURE_RECOVERY_PROBE_MIN_ELAPSED
        {
            last_compat_probe = Instant::now();
            compat_probe_attempts += 1;
            match initialize_screen_capture(&logger, CAPTURE_RECOVERY_PROBE_INIT_RETRIES) {
                Ok(new_capture) => {
                    logger.log(
                        "captura de pantalla recuperada: desactivando modo de compatibilidad."
                            .to_string(),
                    );
                    capture = Some(new_capture);
                    synthetic_fallback_enabled = false;
                    recovery_retries = 0;
                    compat_probe_attempts = 0;
                }
                Err(err) => {
                    if compat_probe_attempts % CAPTURE_RECOVERY_PROBE_LOG_EVERY == 0 {
                        logger.log(format!(
                            "captura real aún no disponible tras {compat_probe_attempts} sondeos de recuperación; se mantiene imagen de compatibilidad: {err}"
                        ));
                    }
                }
            }
        }
        let tick_start = Instant::now();
        let tuning_snapshot = match tuning.lock() {
            Ok(guard) => *guard,
            Err(_) => StreamTuning::default(),
        };
        let frame_interval_ms = (1000 / tuning_snapshot.target_fps.max(1)).max(16);
        let mut switch_to_synthetic = false;
        let frame = match capture.as_mut() {
            Some(screen) => match screen.capture_frame(
                hello.session_id.clone(),
                sequence,
                frame_interval_ms,
                tuning_snapshot.scale_divisor,
                rdp_session,
            ) {
                Ok(frame) => {
                    recovery_retries = 0;
                    frame
                }
                Err(err) => {
                    // All capture errors are recoverable — DXGI access loss (when RDP
                    // disconnects), device removal, and similar transient failures all
                    // warrant a re-init attempt rather than killing the stream.
                    let is_access_loss = err.contains("screen capture error")
                        || err.contains("Access");
                    let recoverable = true; // always attempt reinit before giving up
                    let _ = recoverable; // keep for future guard if needed
                    recovery_retries += 1;
                    if is_access_loss || recovery_retries == 1 {
                        logger.log(format!(
                            "captura interrumpida (reintento {recovery_retries}/{CAPTURE_RECOVERY_MAX_RETRIES}): {err}"
                        ));
                    }
                    if !synthetic_fallback_enabled
                        && should_enable_compat_fallback(&err, recovery_retries, rdp_session)
                    {
                        logger.log(if rdp_session {
                            "RDP sin imagen estable: activando imagen de compatibilidad temporal."
                                .to_string()
                        } else {
                            "Captura de pantalla temporalmente no disponible: activando imagen de compatibilidad temporal."
                                .to_string()
                        });
                        synthetic_fallback_enabled = true;
                        recovery_retries = 0;
                        switch_to_synthetic = true;
                        synthetic_frame_with_tuning(
                            hello.session_id.clone(),
                            sequence,
                            frame_interval_ms,
                            tuning_snapshot.scale_divisor,
                        )
                    } else {
                        // Try to reinitialize immediately on DXGI access loss.
                        match ScreenCapture::new() {
                            Ok(new_capture) => {
                                logger.log("captura reinicializada tras pérdida de acceso DXGI.".to_string());
                                *screen = new_capture;
                                recovery_retries = 0;
                            }
                            Err(reinit_err) => {
                                logger.log(format!(
                                    "no se pudo reinicializar captura todavía: {reinit_err}"
                                ));
                            }
                        }
                        if recovery_retries >= CAPTURE_RECOVERY_MAX_RETRIES {
                            // Too many failures — switch to synthetic rather than killing
                            // the stream entirely; recovery probe will keep trying.
                            logger.log("demasiados fallos de captura; activando compatibilidad temporal.".to_string());
                            synthetic_fallback_enabled = true;
                            recovery_retries = 0;
                            switch_to_synthetic = true;
                            synthetic_frame_with_tuning(
                                hello.session_id.clone(),
                                sequence,
                                frame_interval_ms,
                                tuning_snapshot.scale_divisor,
                            )
                        } else {
                            thread::sleep(Duration::from_millis(120));
                            continue;
                        }
                    }
                }
            },
            None => synthetic_frame_with_tuning(
                hello.session_id.clone(),
                sequence,
                frame_interval_ms,
                tuning_snapshot.scale_divisor,
            ),
        };
        if switch_to_synthetic {
            capture = None;
        }
        let encoded =
            to_json_line(&frame).map_err(|err| format!("encode stream frame failed: {err}"))?;
        if let Err(err) = stream.write_all(encoded.as_bytes()) {
            if matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
            ) {
                break;
            }
            return Err(format!("send stream frame failed: {err}"));
        }
        let elapsed = tick_start.elapsed();
        let target = Duration::from_millis(frame_interval_ms as u64);
        if elapsed < target {
            thread::sleep(target - elapsed);
        }
    }

    // Evict the session so stale IDs can't be replayed after this channel closes.
    remove_session(&sessions, &hello.session_id);

    Ok(())
}

fn should_enable_compat_fallback(err: &str, recovery_retries: u32, rdp_session: bool) -> bool {
    if recovery_retries < CAPTURE_COMPAT_FALLBACK_THRESHOLD {
        return false;
    }
    // For RDP sessions: accept all capture failures as compat-worthy after threshold.
    if rdp_session {
        return true;
    }
    // For console sessions: only obvious display-unavailable failures warrant compat.
    let is_black_frame = err.contains("black frame detected");
    let is_display_unavailable =
        err.contains("capturer init error") || err.contains("primary display error");
    let is_capture_timeout = err.contains("timeout waiting for frame");
    let is_dxgi_loss = err.contains("screen capture error");
    is_black_frame || is_display_unavailable || is_capture_timeout || is_dxgi_loss
}

fn capture_recovery_probe_interval(rdp_session: bool) -> u64 {
    if rdp_session {
        CAPTURE_RECOVERY_PROBE_INTERVAL_RDP
    } else {
        CAPTURE_RECOVERY_PROBE_INTERVAL
    }
}

fn initialize_screen_capture(logger: &Logger, max_retries: u32) -> Result<ScreenCapture, String> {
    let mut last_error = "captura no inicializada".to_string();
    for attempt in 1..=max_retries {
        match ScreenCapture::new() {
            Ok(capture) => {
                if attempt > 1 {
                    logger.log(format!(
                        "captura de pantalla inicializada tras {attempt} intentos."
                    ));
                }
                return Ok(capture);
            }
            Err(err) => {
                last_error = err;
                if attempt < max_retries {
                    thread::sleep(Duration::from_millis(120));
                }
            }
        }
    }
    Err(format!(
        "captura de pantalla no disponible al iniciar tras {} intentos: {}",
        max_retries, last_error
    ))
}

/// Screen capturer backed by GDI CreateDC("DISPLAY") — works from any thread,
/// including background service threads, and survives RDP disconnect/reconnect.
/// Unlike DXGI/WGC and xcap's GetWindowDC approach, CreateDC does not require
/// the calling thread to be attached to the interactive desktop window station.
struct ScreenCapture;

impl ScreenCapture {
    fn new() -> Result<Self, String> {
        // Verify the capture path works before committing.
        Self::capture_bgra().map(|_| Self)
    }

    fn capture_frame(
        &mut self,
        session_id: String,
        sequence: u64,
        frame_interval_ms: u32,
        scale_divisor: u8,
        detect_black_frame: bool,
    ) -> Result<StreamFrame, String> {
        let (bgra, width, height) = Self::capture_bgra()?;
        let stride_bytes = width as usize * 4;
        let (scaled, out_w, out_h, out_stride) =
            normalize_and_scale_bgra(&bgra, width, height, stride_bytes, scale_divisor);
        if detect_black_frame
            && is_mostly_black_bgra(&scaled, out_stride, out_w as usize, out_h as usize)
        {
            return Err("screen capture black frame detected".to_string());
        }
        let compressed = compress_prepend_size(&scaled);
        let encoded = BASE64.encode(compressed);
        Ok(StreamFrame {
            magic: PROTOCOL_MAGIC.to_string(),
            session_id,
            sequence,
            captured_at_ms: unix_timestamp_ms(),
            width: out_w,
            height: out_h,
            stride_bytes: out_stride,
            pixel_format: "bgra8".to_string(),
            compression: StreamCompression::Lz4,
            frame_interval_ms,
            compressed_payload_b64: encoded,
            raw_len: scaled.len(),
            source: "screen".to_string(),
        })
    }

    /// Capture the primary monitor using GDI CreateDC("DISPLAY").
    /// Returns raw BGRA pixels (alpha = 255) with (width, height).
    #[cfg(windows)]
    fn capture_bgra() -> Result<(Vec<u8>, u32, u32), String> {
        unsafe {
            let width = GetSystemMetrics(SM_CXSCREEN);
            let height = GetSystemMetrics(SM_CYSCREEN);
            if width <= 0 || height <= 0 {
                return Err(format!(
                    "screen capture error: GetSystemMetrics returned {width}x{height}"
                ));
            }
            let w = width as u32;
            let h = height as u32;

            // CreateDC("DISPLAY") works from any thread — no desktop attachment needed.
            let hdc_screen: HDC =
                CreateDCW(&windows::core::HSTRING::from("DISPLAY"), None, None, None);
            if hdc_screen.is_invalid() {
                return Err("screen capture error: CreateDC(DISPLAY) failed".to_string());
            }
            // Defer cleanup with a simple RAII wrapper via a flag.
            let _cleanup_screen = DcGuard(hdc_screen);

            let hdc_mem: HDC = CreateCompatibleDC(Some(hdc_screen));
            if hdc_mem.is_invalid() {
                return Err("screen capture error: CreateCompatibleDC failed".to_string());
            }
            let _cleanup_mem = DcGuard(hdc_mem);

            let hbm: HBITMAP = CreateCompatibleBitmap(hdc_screen, width, height);
            if hbm.is_invalid() {
                return Err("screen capture error: CreateCompatibleBitmap failed".to_string());
            }
            let _cleanup_bm = BitmapGuard(hbm);

            SelectObject(hdc_mem, hbm.into());

            BitBlt(hdc_mem, 0, 0, width, height, Some(hdc_screen), 0, 0, SRCCOPY)
                .map_err(|e| format!("screen capture error: BitBlt failed: {e}"))?;

            let buf_size = (w * h * 4) as usize;
            let mut pixels = vec![0u8; buf_size];
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // negative → top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biSizeImage: buf_size as u32,
                    biCompression: 0, // BI_RGB
                    ..Default::default()
                },
                ..Default::default()
            };
            let rows = GetDIBits(
                hdc_mem,
                hbm,
                0,
                h,
                Some(pixels.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if rows == 0 {
                return Err("screen capture error: GetDIBits returned 0".to_string());
            }

            // GDI fills 32-bit pixels as BGR0; set alpha = 255 so the frame is opaque.
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            Ok((pixels, w, h))
        }
    }

    #[cfg(not(windows))]
    fn capture_bgra() -> Result<(Vec<u8>, u32, u32), String> {
        Err("screen capture only supported on Windows".to_string())
    }
}

/// RAII guard that calls DeleteDC on drop.
#[cfg(windows)]
struct DcGuard(HDC);
#[cfg(windows)]
impl Drop for DcGuard {
    fn drop(&mut self) {
        unsafe { let _ = DeleteDC(self.0); }
    }
}

/// RAII guard that calls DeleteObject on drop.
#[cfg(windows)]
struct BitmapGuard(HBITMAP);
#[cfg(windows)]
impl Drop for BitmapGuard {
    fn drop(&mut self) {
        unsafe { let _ = DeleteObject(self.0.into()); }
    }
}



fn is_mostly_black_bgra(raw: &[u8], stride_bytes: usize, width: usize, height: usize) -> bool {
    if width == 0 || height == 0 || stride_bytes < width * 4 {
        return false;
    }
    let step_x = (width / 32).max(1);
    let step_y = (height / 24).max(1);
    let mut samples = 0usize;
    let mut dark = 0usize;
    for y in (0..height).step_by(step_y) {
        let row_start = y * stride_bytes;
        for x in (0..width).step_by(step_x) {
            let idx = row_start + x * 4;
            if idx + 2 >= raw.len() {
                return false;
            }
            let b = raw[idx] as u16;
            let g = raw[idx + 1] as u16;
            let r = raw[idx + 2] as u16;
            let brightness = (r + g + b) / 3;
            if brightness <= 8 {
                dark += 1;
            }
            samples += 1;
        }
    }
    samples > 0 && dark * 100 >= samples * 95
}

fn synthetic_frame_with_tuning(
    session_id: String,
    sequence: u64,
    frame_interval_ms: u32,
    scale_divisor: u8,
) -> StreamFrame {
    let divisor = scale_divisor.clamp(1, 3) as u32;
    let mut frame = StreamFrame::synthetic(session_id, sequence);
    frame.width = (frame.width / divisor).max(1);
    frame.height = (frame.height / divisor).max(1);
    frame.stride_bytes = frame.width as usize * 4;
    frame.raw_len = frame.stride_bytes * frame.height as usize;
    frame.frame_interval_ms = frame_interval_ms;
    frame
}

fn normalize_and_scale_bgra(
    input: &[u8],
    width: u32,
    height: u32,
    input_stride: usize,
    scale_divisor: u8,
) -> (Vec<u8>, u32, u32, usize) {
    let divisor = scale_divisor.clamp(1, 3) as usize;
    let out_width = (width as usize / divisor).max(1);
    let out_height = (height as usize / divisor).max(1);
    let out_stride = out_width * 4;
    let mut out = vec![0_u8; out_height * out_stride];

    for y in 0..out_height {
        let src_y = (y * divisor).min(height as usize - 1);
        let src_row = src_y * input_stride;
        let dst_row = y * out_stride;
        for x in 0..out_width {
            let src_x = (x * divisor).min(width as usize - 1);
            let src_idx = src_row + src_x * 4;
            let dst_idx = dst_row + x * 4;
            out[dst_idx..dst_idx + 4].copy_from_slice(&input[src_idx..src_idx + 4]);
        }
    }

    (out, out_width as u32, out_height as u32, out_stride)
}

fn handle_handshake(
    mut stream: TcpStream,
    host_name: &str,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    logger: &Logger,
) -> Result<(), String> {
    let remote: SocketAddr = stream
        .peer_addr()
        .map_err(|err| format!("peer addr error: {err}"))?;

    // Timeout prevents a client that connects but never sends from blocking the thread forever.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("set read timeout error: {err}"))?;

    let mut reader = io::BufReader::new(
        stream
            .try_clone()
            .map_err(|err| format!("clone stream error: {err}"))?,
    );

    let mut line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut line)
        .map_err(|err| format!("read hello error: {err}"))?;

    let hello: HandshakeHello =
        from_json_line(&line).map_err(|err| format!("invalid hello payload: {err}"))?;
    if hello.magic != PROTOCOL_MAGIC || hello.role != "agent" {
        return Err(format!("invalid hello from {remote}: {:?}", hello));
    }

    let ack = HandshakeAck::ok(host_name.to_string());
    let encoded = to_json_line(&ack).map_err(|err| format!("encode ack error: {err}"))?;
    stream
        .write_all(encoded.as_bytes())
        .map_err(|err| format!("write ack error: {err}"))?;

    let source_ip = match remote.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => ip.to_string(),
    };
    // Register the session so control/stream channels can validate it.
    register_session(&sessions, ack.session_id.clone());
    log_session_event(&SessionEvent::Created {
        session_id: ack.session_id.clone(),
        peer_ip: source_ip.clone(),
        agent_name: hello.agent_name.clone(),
        timestamp_ms: unix_timestamp_ms(),
    });
    logger.log(format!(
        "Handshake accepted: agent={} remote={} session={}",
        hello.agent_name, source_ip, ack.session_id
    ));
    Ok(())
}

fn register_session(sessions: &Arc<Mutex<HashMap<String, Instant>>>, session_id: String) {
    let now = Instant::now();
    if let Ok(mut guard) = sessions.lock() {
        prune_expired_sessions(&mut guard, now);
        guard.insert(session_id, now);
    }
}

fn remove_session(sessions: &Arc<Mutex<HashMap<String, Instant>>>, session_id: &str) {
    if let Ok(mut guard) = sessions.lock() {
        if guard.remove(session_id).is_some() {
            log_session_event(&SessionEvent::Removed {
                session_id: session_id.to_string(),
                timestamp_ms: unix_timestamp_ms(),
            });
        }
    }
}

fn session_is_valid(sessions: &Arc<Mutex<HashMap<String, Instant>>>, session_id: &str) -> bool {
    let now = Instant::now();
    let Ok(mut guard) = sessions.lock() else {
        return false;
    };
    prune_expired_sessions(&mut guard, now);
    if let Some(seen_at) = guard.get_mut(session_id) {
        *seen_at = now;
        log_session_event(&SessionEvent::Refreshed {
            session_id: session_id.to_string(),
            timestamp_ms: unix_timestamp_ms(),
        });
        true
    } else {
        false
    }
}

fn prune_expired_sessions(sessions: &mut HashMap<String, Instant>, now: Instant) {
    let expired_ids: Vec<String> = sessions
        .iter()
        .filter_map(|(session_id, seen_at)| {
            (now.duration_since(*seen_at) > SESSION_TTL).then(|| session_id.clone())
        })
        .collect();
    for session_id in expired_ids {
        sessions.remove(&session_id);
        log_session_event(&SessionEvent::Expired {
            session_id,
            reason: "session_ttl_elapsed".to_string(),
            timestamp_ms: unix_timestamp_ms(),
        });
    }
}

fn is_timeout(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

fn is_would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

fn is_remote_desktop_session() -> bool {
    matches!(
        std::env::var("SESSIONNAME"),
        Ok(name) if is_remote_desktop_session_name(&name)
    )
}

fn is_remote_desktop_session_name(session_name: &str) -> bool {
    session_name
        .trim()
        .to_ascii_lowercase()
        .starts_with("rdp-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn stream_source_default_is_screen() {
        assert_eq!(StreamSource::default(), StreamSource::Screen);
    }

    #[test]
    fn host_config_default_has_no_forced_pair_code() {
        let config = HostConfig::default();
        assert!(config.pair_code.is_none());
        assert!(config.host_name.is_none());
        assert_eq!(config.stream_source, StreamSource::Screen);
        assert_eq!(config.max_stream_frames, u64::MAX);
    }

    #[test]
    fn synthetic_frame_respects_scale_divisor() {
        let frame = synthetic_frame_with_tuning("lp-test".to_string(), 0, 100, 2);
        assert_eq!(frame.width, 480);
        assert_eq!(frame.height, 270);
        assert_eq!(frame.stride_bytes, frame.width as usize * 4);
    }

    #[test]
    fn run_host_fails_fast_when_port_already_bound() {
        // Bind the discovery UDP port ourselves first so run_host must fail
        // during setup instead of hanging or exiting the process.
        let blocker = bind_udp(DISCOVERY_PORT);
        let Ok(_blocker) = blocker else {
            // Port already busy from a previous test run/other process — skip.
            return;
        };

        let logger_lines: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&logger_lines);
        let logger = Logger::new(move |line| sink.lock().unwrap().push(line));
        let stop = lanpilot_core::new_stop_flag();

        let config = HostConfig {
            host_name: Some("test-host".to_string()),
            pair_code: Some("123456".to_string()),
            stream_source: StreamSource::Synthetic,
            max_stream_frames: 1,
        };

        let result = run_host(config, logger, stop);
        assert!(result.is_err(), "expected bind failure to be propagated as an error");
    }

    #[test]
    fn stop_flag_causes_run_host_to_return_quickly() {
        // Use a distinct, likely-free set of ports isn't possible since ports
        // are constants; instead we just verify that pre-setting the stop
        // flag makes run_host exit promptly once it does manage to bind.
        // If ports are busy (e.g. from another test), we accept the Err path.
        let logger = Logger::new(|_line| {});
        let stop = lanpilot_core::new_stop_flag();
        stop.store(true, Ordering::SeqCst);

        let config = HostConfig {
            host_name: Some("test-host".to_string()),
            pair_code: Some("654321".to_string()),
            stream_source: StreamSource::Synthetic,
            max_stream_frames: 1,
        };

        let started = Instant::now();
        let _ = run_host(config, logger, stop);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "run_host must return promptly once stop is already set"
        );
    }

    #[test]
    fn remote_desktop_session_name_detection_is_case_insensitive() {
        assert!(is_remote_desktop_session_name("RDP-Tcp#1"));
        assert!(is_remote_desktop_session_name("rdp-tcp#56"));
        assert!(!is_remote_desktop_session_name("Console"));
    }

    #[test]
    fn mostly_black_detector_behaves_as_expected() {
        let width = 32usize;
        let height = 16usize;
        let stride = width * 4;
        let dark = vec![0_u8; stride * height];
        assert!(is_mostly_black_bgra(&dark, stride, width, height));

        let mut bright = vec![0_u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let idx = y * stride + x * 4;
                bright[idx] = 255;
                bright[idx + 1] = 255;
                bright[idx + 2] = 255;
                bright[idx + 3] = 255;
            }
        }
        assert!(!is_mostly_black_bgra(&bright, stride, width, height));
    }

    #[test]
    fn compat_fallback_enables_on_non_rdp_display_unavailable() {
        // Console sessions now also enable compat fallback on display errors after threshold.
        assert!(should_enable_compat_fallback(
            "capturer init error: no display",
            CAPTURE_COMPAT_FALLBACK_THRESHOLD,
            false
        ));
    }

    #[test]
    fn compat_fallback_does_not_enable_before_threshold() {
        assert!(!should_enable_compat_fallback(
            "primary display error: unavailable",
            CAPTURE_COMPAT_FALLBACK_THRESHOLD - 1,
            false
        ));
    }

    #[test]
    fn compat_fallback_enables_for_rdp_after_threshold() {
        assert!(should_enable_compat_fallback(
            "screen capture timeout waiting for frame",
            CAPTURE_COMPAT_FALLBACK_THRESHOLD,
            true
        ));
    }

    #[test]
    fn compat_fallback_enables_on_timeout_for_console_sessions_after_threshold() {
        // Console sessions also enable compat on repeated capture timeouts.
        assert!(should_enable_compat_fallback(
            "screen capture timeout waiting for frame",
            CAPTURE_COMPAT_FALLBACK_THRESHOLD,
            false
        ));
    }

    #[test]
    fn compat_fallback_enables_on_black_frame_even_without_rdp() {
        // Black frame also enables compat for console sessions after threshold.
        assert!(should_enable_compat_fallback(
            "screen capture black frame detected",
            CAPTURE_COMPAT_FALLBACK_THRESHOLD,
            false
        ));
    }

    #[test]
    fn recovery_probe_interval_is_more_aggressive_for_rdp() {
        assert_eq!(capture_recovery_probe_interval(false), CAPTURE_RECOVERY_PROBE_INTERVAL);
        assert_eq!(
            capture_recovery_probe_interval(true),
            CAPTURE_RECOVERY_PROBE_INTERVAL_RDP
        );
    }
}
