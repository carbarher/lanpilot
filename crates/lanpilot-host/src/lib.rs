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

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lanpilot_core::{
    CONTROL_PORT, ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse,
    HANDSHAKE_PORT, HandshakeAck, HandshakeHello, IpRateLimiter, Logger, PRODUCT_NAME,
    PROTOCOL_MAGIC, STREAM_PORT, AUDIO_PORT, SessionEvent, StopFlag, StreamCompression, StreamFrame,
    StreamHello, TAGLINE, from_json_line, generate_pair_code, is_stopped, local_ipv4,
    log_session_event, normalize_pair_code, to_json_line, unix_timestamp_ms,
};
use lz4_flex::compress_prepend_size;

#[cfg(windows)]
use windows::{
    core::BOOL,
    Win32::{
        Foundation::RECT,
        Graphics::Gdi::{
            BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
            CreateDCW, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, HBITMAP, HDC, SelectObject,
            SRCCOPY,
        },
        UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
        },
    },
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
    use_rgb565: bool,
}

impl Default for StreamTuning {
    fn default() -> Self {
        Self {
            target_fps: 10,
            scale_divisor: 1,
            use_rgb565: false,
        }
    }
}

const MAX_HANDSHAKE_CONNECTIONS: usize = 16;
const MAX_CONTROL_CONNECTIONS: usize = 16;
const MAX_STREAM_CONNECTIONS: usize = 8;
const SESSION_TTL: Duration = Duration::from_secs(300);
/// How often accept/read loops wake up to check the stop flag.
const POLL_INTERVAL: Duration = Duration::from_millis(450);
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

fn bind_tcp(ip: Ipv4Addr, port: u16) -> Result<TcpListener, String> {
    TcpListener::bind((ip, port)).map_err(|_| {
        format!(
            "el puerto {port} ya está en uso. Cierra otras instancias de LanPilot e inténtalo de nuevo."
        )
    })
}

fn bind_udp(ip: Ipv4Addr, port: u16) -> Result<UdpSocket, String> {
    UdpSocket::bind((ip, port)).map_err(|_| {
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
#[cfg(windows)]
fn make_process_dpi_aware() {
    unsafe {
        #[link(name = "shcore")]
        unsafe extern "system" {
            fn SetProcessDpiAwareness(value: i32) -> i32;
        }
        let _ = SetProcessDpiAwareness(2); // PROCESS_PER_MONITOR_DPI_AWARE

        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetProcessDPIAware() -> i32;
        }
        let _ = SetProcessDPIAware();
    }
}

#[cfg(windows)]
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> i32 {
    unsafe {
        BlockInput(0);
        SendNotifyMessageW(0xFFFF as *mut std::ffi::c_void, 0x0112, 0xF170, -1);
    }
    0
}

#[cfg(windows)]
fn set_keep_awake(enabled: bool) {
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetThreadExecutionState(esFlags: u32) -> u32;
        }
        if enabled {
            SetThreadExecutionState(0x80000000 | 0x00000001 | 0x00000002);
        } else {
            SetThreadExecutionState(0x80000000);
        }
    }
}
#[cfg(not(windows))]
fn set_keep_awake(_enabled: bool) {}

struct KeepAwakeGuard;

impl KeepAwakeGuard {
    fn new() -> Self {
        set_keep_awake(true);
        Self
    }
}

impl Drop for KeepAwakeGuard {
    fn drop(&mut self) {
        set_keep_awake(false);
    }
}

pub fn run_host(config: HostConfig, logger: Logger, stop: StopFlag) -> Result<(), String> {
    #[cfg(windows)]
    {
        make_process_dpi_aware();
        unsafe {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetConsoleCtrlHandler(HandlerRoutine: Option<unsafe extern "system" fn(u32) -> i32>, Add: i32) -> i32;
            }
            let _ = SetConsoleCtrlHandler(Some(ctrl_handler), 1);
        }
    }

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
        "[Info] UDP {DISCOVERY_PORT}  TCP {HANDSHAKE_PORT}/{CONTROL_PORT}/{STREAM_PORT}/{AUDIO_PORT}  — {host_name} ({host_ipv4})"
    ));
    logger.log("Esperando al agente...".to_string());

    // Bind all sockets up front so setup failures are reported immediately
    // and atomically, instead of surfacing later from a background thread.
    let discovery_socket = bind_udp(host_ipv4, DISCOVERY_PORT)?;
    let handshake_listener = bind_tcp(host_ipv4, HANDSHAKE_PORT)?;
    let control_listener = bind_tcp(host_ipv4, CONTROL_PORT)?;
    let stream_listener = bind_tcp(host_ipv4, STREAM_PORT)?;
    let audio_listener = bind_tcp(host_ipv4, AUDIO_PORT)?;

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
            use_rgb565: false,
        }
    } else {
        StreamTuning::default()
    };
    let tuning = Arc::new(Mutex::new(initial_tuning));
    let active_sessions: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let last_sync_clipboard = Arc::new(Mutex::new(String::new()));
    let control_limiter = Arc::new(ConnectionLimiter::new(MAX_CONTROL_CONNECTIONS, "control"));
    let stream_limiter = Arc::new(ConnectionLimiter::new(MAX_STREAM_CONNECTIONS, "stream"));
    let handshake_limiter = Arc::new(ConnectionLimiter::new(MAX_HANDSHAKE_CONNECTIONS, "handshake"));

    let initial_rect = {
        #[cfg(windows)]
        {
            unsafe {
                (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
            }
        }
        #[cfg(not(windows))]
        (0, 0, 1920, 1080)
    };
    let active_capture_rect = Arc::new(Mutex::new(initial_rect));
    let active_monitor_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));

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
        let control_clipboard = Arc::clone(&last_sync_clipboard);
        let active_rect = Arc::clone(&active_capture_rect);
        let monitor_idx = Arc::clone(&active_monitor_index);
        handles.push(thread::spawn(move || {
            run_control_server(
                control_listener,
                control_tuning,
                sessions,
                control_limiter,
                logger,
                stop,
                control_clipboard,
                active_rect,
                monitor_idx,
            )
        }));
    }
    {
        let sessions = Arc::clone(&active_sessions);
        let logger = logger.clone();
        let stop = Arc::clone(&stop);
        let stream_source = config.stream_source;
        let max_stream_frames = config.max_stream_frames;
        let stream_tuning = Arc::clone(&tuning);
        let stream_clipboard = Arc::clone(&last_sync_clipboard);
        let active_rect = Arc::clone(&active_capture_rect);
        let monitor_idx = Arc::clone(&active_monitor_index);
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
                stream_clipboard,
                active_rect,
                monitor_idx,
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
    {
        let logger = logger.clone();
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            run_audio_server(audio_listener, logger, stop)
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

        if !lanpilot_core::is_private_ip(source.ip()) {
            logger.log(format!("Ignorando sondeo de descubrimiento de IP pública no autorizada: {}", source.ip()));
            continue;
        }

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
            Ok((stream, _addr)) => {
                let _ = stream.set_nodelay(true);
                stream
            }
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

        if !lanpilot_core::is_private_ip(peer_ip) {
            logger.log(format!("Conexión de handshake rechazada de IP pública no autorizada: {}", peer_ip));
            continue;
        }

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
    last_sync_clipboard: Arc<Mutex<String>>,
    active_capture_rect: Arc<Mutex<(i32, i32, i32, i32)>>,
    active_monitor_index: Arc<std::sync::atomic::AtomicUsize>,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        logger.log(format!("control set_nonblocking error: {err}"));
        return;
    }

    while !is_stopped(&stop) {
        let stream = match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = stream.set_nodelay(true);
                stream
            }
            Err(err) if is_would_block(&err) => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(err) => {
                logger.log(format!("control incoming connection error: {err}"));
                continue;
            }
        };

        if let Ok(addr) = stream.peer_addr() {
            if !lanpilot_core::is_private_ip(addr.ip()) {
                logger.log(format!("Conexión de control rechazada de IP pública no autorizada: {}", addr.ip()));
                continue;
            }
        } else {
            continue;
        }

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
        let clipboard_clone = Arc::clone(&last_sync_clipboard);
        let active_rect = Arc::clone(&active_capture_rect);
        let monitor_idx = Arc::clone(&active_monitor_index);
        thread::spawn(move || {
            let _permit = permit;
            handle_control_stream(
                stream,
                tuning,
                sessions,
                logger,
                stop,
                clipboard_clone,
                active_rect,
                monitor_idx,
            )
        });
    }
}

struct PrivacyGuard {
    active: bool,
    logger: Logger,
}

impl Drop for PrivacyGuard {
    fn drop(&mut self) {
        if self.active {
            #[cfg(windows)]
            unsafe {
                BlockInput(0);
                SendNotifyMessageW(0xFFFF as *mut std::ffi::c_void, 0x0112, 0xF170, -1);
            }
            self.logger.log("[Privacidad] Modo Privacidad restaurado automáticamente al cerrar conexión.".to_string());
        }
    }
}

fn handle_control_stream(
    stream: TcpStream,
    tuning: Arc<Mutex<StreamTuning>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    logger: Logger,
    stop: StopFlag,
    last_sync_clipboard: Arc<Mutex<String>>,
    active_capture_rect: Arc<Mutex<(i32, i32, i32, i32)>>,
    active_monitor_index: Arc<std::sync::atomic::AtomicUsize>,
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

    let mut privacy_guard = PrivacyGuard { active: false, logger: logger.clone() };

    let mut local_clipboard = arboard::Clipboard::new().ok();
    let mut reader = io::BufReader::new(stream);
    let mut writer = reader.get_ref().try_clone().ok();

    let mut last_clipboard_image_hash: Option<u64> = None;
    let mut last_clipboard_image_check = Instant::now();
    let mut last_ping_sent = Instant::now();
    let mut pings_unanswered = 0;

    while !is_stopped(&stop) {
        let now = Instant::now();

        // 1. Keep-Alive Ping cada 2 segundos
        if now.duration_since(last_ping_sent) >= Duration::from_secs(2) {
            last_ping_sent = now;
            pings_unanswered += 1;
            if pings_unanswered >= 3 {
                logger.log(format!("Desconexión por inactividad (Keep-Alive): {} no responde tras 6s.", peer));
                break;
            }
            if let Some(ref mut w) = writer {
                use std::io::Write;
                let event = ControlEvent::Ping { timestamp_ms: unix_timestamp_ms() as u64 };
                let control_frame = ControlFrame::new("ping-frame".to_string(), vec![event]);
                if let Ok(line) = to_json_line(&control_frame) {
                    let _ = w.write_all(line.as_bytes());
                }
            }
        }

        // 2. Sincronización de Imágenes en el Portapapeles cada 1.5 segundos
        if now.duration_since(last_clipboard_image_check) >= Duration::from_millis(1500) {
            last_clipboard_image_check = now;
            if let Some(ref mut w) = writer {
                if let Some(ref mut cb) = local_clipboard {
                    if let Ok(img) = cb.get_image() {
                        let hash = {
                            let mut h = img.width as u64 ^ img.height as u64;
                            let len = img.bytes.len();
                            if len > 0 {
                                h = h.wrapping_add(len as u64);
                                h = h.wrapping_add(img.bytes[0] as u64);
                                h = h.wrapping_add(img.bytes[len / 2] as u64);
                                h = h.wrapping_add(img.bytes[len - 1] as u64);
                            }
                            h
                        };
                        if last_clipboard_image_hash != Some(hash) {
                            last_clipboard_image_hash = Some(hash);
                            
                            let (final_bytes, final_w, final_h) = if img.width > 1920 || img.height > 1080 {
                                let scale_w = img.width as f64 / 1920.0;
                                let scale_h = img.height as f64 / 1080.0;
                                let scale = scale_w.max(scale_h);
                                let new_w = (img.width as f64 / scale) as usize;
                                let new_h = (img.height as f64 / scale) as usize;
                                let mut resized = vec![0u8; new_w * new_h * 4];
                                for y in 0..new_h {
                                    let src_y = ((y as f64 * scale) as usize).min(img.height - 1);
                                    for x in 0..new_w {
                                        let src_x = ((x as f64 * scale) as usize).min(img.width - 1);
                                        let src_idx = (src_y * img.width + src_x) * 4;
                                        let dst_idx = (y * new_w + x) * 4;
                                        resized[dst_idx..dst_idx + 4].copy_from_slice(&img.bytes[src_idx..src_idx + 4]);
                                    }
                                }
                                (resized, new_w, new_h)
                            } else {
                                (img.bytes.into_owned(), img.width, img.height)
                            };

                            let encoded = BASE64.encode(&final_bytes);
                            let event = ControlEvent::ClipboardImage {
                                width: final_w,
                                height: final_h,
                                rgba_payload_b64: encoded,
                            };
                            let control_frame = ControlFrame::new("clipboard-image".to_string(), vec![event]);
                            if let Ok(line) = to_json_line(&control_frame) {
                                use std::io::Write;
                                let _ = w.write_all(line.as_bytes());
                            }
                        }
                    }
                }
            }
        }

        let mut line = String::new();
        let bytes_read = match std::io::BufRead::read_line(&mut reader, &mut line) {
            Ok(read) => read,
            Err(err) if is_would_block(&err) || is_timeout(&err) => continue,
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
        pings_unanswered = 0;
        for event in &frame.events {
            if let ControlEvent::StreamFeedback {
                target_fps,
                scale_divisor,
                avg_latency_ms: _,
                jitter_ms,
                echo_captured_at_ms,
            } = event
            {
                let now = unix_timestamp_ms() as u64;
                let real_latency_ms = if now >= *echo_captured_at_ms && *echo_captured_at_ms > 0 {
                    let rtt = now - *echo_captured_at_ms;
                    (rtt / 2) as u32
                } else {
                    0u32
                };

                let mut guard = match tuning.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        logger.log("failed to lock stream tuning".to_string());
                        continue;
                    }
                };
                let next_fps = (*target_fps).clamp(3, 60);
                let next_scale = (*scale_divisor).clamp(1, 3);
                if guard.target_fps != next_fps || guard.scale_divisor != next_scale {
                    logger.log(format!(
                        "Adaptive stream update: fps {}->{} scale {}->{} (lat={}ms jitter={}ms)",
                        guard.target_fps, next_fps, guard.scale_divisor, next_scale, real_latency_ms, jitter_ms
                    ));
                    guard.target_fps = next_fps;
                    guard.scale_divisor = next_scale;
                }
            }
            if let ControlEvent::Clipboard { text } = event {
                if let Some(ref mut cb) = local_clipboard {
                    let _ = cb.set_text(text.clone());
                    if let Ok(mut guard) = last_sync_clipboard.lock() {
                        *guard = text.clone();
                    }
                    logger.log(format!("Sincronización de portapapeles: recibidos {} bytes", text.len()));
                }
            }
            if let ControlEvent::MouseMove { dx, dy } = event {
                #[cfg(windows)]
                unsafe {
                    let (offset_x, offset_y) = if let Ok(guard) = active_capture_rect.lock() {
                        (guard.0, guard.1)
                    } else {
                        (0, 0)
                    };
                    let _ = SetCursorPos(*dx + offset_x, *dy + offset_y);
                }
            }
            if let ControlEvent::MouseMoveRelative { dx, dy } = event {
                #[cfg(windows)]
                {
                    send_mouse_input(0x0001, *dx, *dy, 0);
                }
            }
            if let ControlEvent::CycleMonitor = event {
                let monitors = get_connected_monitors();
                let total_monitors = monitors.len();
                
                let next_idx = active_monitor_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                
                let cycle_len = if total_monitors > 1 { total_monitors + 1 } else { total_monitors };
                let final_idx = if cycle_len > 0 { next_idx % cycle_len } else { 0 };
                
                active_monitor_index.store(final_idx, std::sync::atomic::Ordering::SeqCst);
                
                let new_rect = if final_idx < total_monitors {
                    let m = monitors[final_idx];
                    logger.log(format!("Rotado a monitor físico {}: left={}, top={}, width={}, height={}", final_idx + 1, m.0, m.1, m.2, m.3));
                    m
                } else {
                    #[cfg(windows)]
                    unsafe {
                        let vx = GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX(76)); // SM_XVIRTUALSCREEN
                        let vy = GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX(77)); // SM_YVIRTUALSCREEN
                        let vw = GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX(78)); // SM_CXVIRTUALSCREEN
                        let vh = GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX(79)); // SM_CYVIRTUALSCREEN
                        logger.log(format!("Rotado a Escritorio Virtual Completo: left={}, top={}, width={}, height={}", vx, vy, vw, vh));
                        (vx, vy, vw, vh)
                    }
                    #[cfg(not(windows))]
                    (0, 0, 1920, 1080)
                };
                
                if let Ok(mut guard) = active_capture_rect.lock() {
                    *guard = new_rect;
                }
            }
            if let ControlEvent::MouseButton { button, pressed } = event {
                #[cfg(windows)]
                {
                    let dw_flags = match (button.as_str(), *pressed) {
                        ("left", true) => 0x0002, // MOUSEEVENTF_LEFTDOWN
                        ("left", false) => 0x0004, // MOUSEEVENTF_LEFTUP
                        ("right", true) => 0x0008, // MOUSEEVENTF_RIGHTDOWN
                        ("right", false) => 0x0010, // MOUSEEVENTF_RIGHTUP
                        ("middle", true) => 0x0020, // MOUSEEVENTF_MIDDLEDOWN
                        ("middle", false) => 0x0040, // MOUSEEVENTF_MIDDLEUP
                        _ => 0,
                    };
                    if dw_flags != 0 {
                        send_mouse_input(dw_flags, 0, 0, 0);
                    }
                }
            }
            if let ControlEvent::Key { key, pressed } = event {
                #[cfg(windows)]
                unsafe {
                    let ctrl_active = GetKeyState(0x11) < 0;
                    let alt_active = GetKeyState(0x12) < 0;
                    let win_active = GetKeyState(0x5B) < 0 || GetKeyState(0x5C) < 0;
                    let any_modifier = ctrl_active || alt_active || win_active;
                    
                    let is_normal_printable_key = key.len() == 1 || key.starts_with("Key") || key.starts_with("NumPad");
                    
                    if !is_normal_printable_key || any_modifier {
                        if let Some(vk) = map_key_to_vk(key) {
                            let dw_flags = if *pressed { 0 } else { 2 }; // KEYEVENTF_KEYUP = 2
                            send_keyboard_input(vk as u16, 0, dw_flags);
                        }
                    }
                }
            }
            if let ControlEvent::UnicodeChar { ch } = event {
                if ch != " " {
                    #[cfg(windows)]
                    {
                        let chars_utf16: Vec<u16> = ch.encode_utf16().collect();
                        for code in chars_utf16 {
                            send_keyboard_input(0, code, 4); // KEYEVENTF_UNICODE = 4
                            send_keyboard_input(0, code, 6); // KEYEVENTF_UNICODE | KEYEVENTF_KEYUP = 6
                        }
                    }
                }
            }
            if let ControlEvent::KeyLockState { caps_lock, num_lock } = event {
                #[cfg(windows)]
                unsafe {
                    let local_caps = (GetKeyState(0x14) & 1) != 0;
                    if local_caps != *caps_lock {
                        send_keyboard_input(0x14, 0, 0);
                        send_keyboard_input(0x14, 0, 2);
                    }
                    let local_num = (GetKeyState(0x90) & 1) != 0;
                    if local_num != *num_lock {
                        send_keyboard_input(0x90, 0, 0);
                        send_keyboard_input(0x90, 0, 2);
                    }
                }
            }
            if let ControlEvent::FileChunk { filename, offset, total_size, data_b64 } = event {
                #[cfg(windows)]
                {
                    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
                    let downloads_dir = format!("{}\\Desktop\\LanPilot-Downloads", home);
                    let _ = std::fs::create_dir_all(&downloads_dir);
                    
                    let safe_filename = std::path::Path::new(filename)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("descarga");
                        
                    let target_path = format!("{}\\{}", downloads_dir, safe_filename);
                    
                    if let Ok(decoded_data) = BASE64.decode(data_b64) {
                        let write_result = {
                            use std::os::windows::fs::FileExt;
                            std::fs::OpenOptions::new()
                                .write(true)
                                .create(true)
                                .open(&target_path)
                                .and_then(|file| {
                                    file.seek_write(&decoded_data, *offset)
                                })
                        };
                        
                        match write_result {
                            Ok(_) => {
                                let written = *offset + decoded_data.len() as u64;
                                let percent = if *total_size > 0 { written * 100 / *total_size } else { 100 };
                                if percent % 10 == 0 || percent == 100 {
                                    logger.log(format!("Transferencia de {}: {}% completado ({}/{} bytes)", safe_filename, percent, written, *total_size));
                                }
                                
                                if let Some(ref mut w) = writer {
                                    let ack_event = ControlEvent::FileChunkAck {
                                        filename: filename.clone(),
                                        offset: *offset,
                                    };
                                    let control_frame = ControlFrame::new(frame.session_id.clone(), vec![ack_event]);
                                    if let Ok(line) = to_json_line(&control_frame) {
                                        use std::io::Write;
                                        let _ = w.write_all(line.as_bytes());
                                    }
                                }
                            }
                            Err(err) => {
                                logger.log(format!("Error escribiendo chunk de archivo: {err}"));
                            }
                        }
                    }
                }
            }
            if let ControlEvent::SetVideoFormat { use_rgb565 } = event {
                if let Ok(mut guard) = tuning.lock() {
                    guard.use_rgb565 = *use_rgb565;
                    logger.log(format!("Formato de video actualizado en caliente: use_rgb565={}", *use_rgb565));
                }
            }
            if let ControlEvent::Ping { timestamp_ms } = event {
                pings_unanswered = 0;
                if let Some(ref mut w) = writer {
                    use std::io::Write;
                    let response = ControlEvent::Pong { timestamp_ms: *timestamp_ms };
                    let control_frame = ControlFrame::new(frame.session_id.clone(), vec![response]);
                    if let Ok(line) = to_json_line(&control_frame) {
                        let _ = w.write_all(line.as_bytes());
                    }
                }
            }
            if let ControlEvent::Pong { .. } = event {
                pings_unanswered = 0;
            }
            if let ControlEvent::ClipboardImage { width, height, rgba_payload_b64 } = event {
                pings_unanswered = 0;
                if let Some(ref mut cb) = local_clipboard {
                    if let Ok(decoded_bytes) = BASE64.decode(rgba_payload_b64) {
                        let img_data = arboard::ImageData {
                            width: *width,
                            height: *height,
                            bytes: std::borrow::Cow::Owned(decoded_bytes),
                        };
                        let hash = {
                            let mut h = img_data.width as u64 ^ img_data.height as u64;
                            let len = img_data.bytes.len();
                            if len > 0 {
                                h = h.wrapping_add(len as u64);
                                h = h.wrapping_add(img_data.bytes[0] as u64);
                                h = h.wrapping_add(img_data.bytes[len / 2] as u64);
                                h = h.wrapping_add(img_data.bytes[len - 1] as u64);
                            }
                            h
                        };
                        last_clipboard_image_hash = Some(hash);
                        let _ = cb.set_image(img_data);
                        logger.log(format!("Sincronización de portapapeles: recibida imagen de {}x{}", *width, *height));
                    }
                }
            }
            if let ControlEvent::TogglePrivacyMode { enabled } = event {
                pings_unanswered = 0;
                privacy_guard.active = *enabled;
                #[cfg(windows)]
                unsafe {
                    if *enabled {
                        BlockInput(1);
                        SendNotifyMessageW(0xFFFF as *mut std::ffi::c_void, 0x0112, 0xF170, 2);
                        logger.log("[Privacidad] Activada. Pantalla física apagada y entrada local bloqueada.".to_string());
                    } else {
                        BlockInput(0);
                        SendNotifyMessageW(0xFFFF as *mut std::ffi::c_void, 0x0112, 0xF170, -1);
                        logger.log("[Privacidad] Desactivada. Pantallas encendidas y entrada local restaurada.".to_string());
                    }
                }
            }
        }
    }
    
    #[cfg(windows)]
    {
        send_keyboard_input(0x11, 0, 2); // Ctrl Up (VK_CONTROL = 0x11, KEYEVENTF_KEYUP = 2)
        send_keyboard_input(0x10, 0, 2); // Shift Up (VK_SHIFT = 0x10)
        send_keyboard_input(0x12, 0, 2); // Alt Up (VK_MENU = 0x12)
        send_keyboard_input(0x5B, 0, 2); // Win Up (VK_LWIN = 0x5B)
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
    last_sync_clipboard: Arc<Mutex<String>>,
    active_capture_rect: Arc<Mutex<(i32, i32, i32, i32)>>,
    active_monitor_index: Arc<std::sync::atomic::AtomicUsize>,
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
            Ok((stream, _addr)) => {
                let _ = stream.set_nodelay(true);
                stream
            }
            Err(err) if is_would_block(&err) => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(err) => {
                logger.log(format!("stream incoming connection error: {err}"));
                continue;
            }
        };

        if let Ok(addr) = stream.peer_addr() {
            if !lanpilot_core::is_private_ip(addr.ip()) {
                logger.log(format!("Conexión de stream rechazada de IP pública no autorizada: {}", addr.ip()));
                continue;
            }
        } else {
            continue;
        }

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
        let clipboard_clone = Arc::clone(&last_sync_clipboard);
        let active_rect = Arc::clone(&active_capture_rect);
        let monitor_idx = Arc::clone(&active_monitor_index);
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
                clipboard_clone,
                active_rect,
                monitor_idx,
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
    _max_stream_frames: u64,
    pre_warmed_capture: Option<ScreenCapture>,
    last_sync_clipboard: Arc<Mutex<String>>,
    active_capture_rect: Arc<Mutex<(i32, i32, i32, i32)>>,
    active_monitor_index: Arc<std::sync::atomic::AtomicUsize>,
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

    let _awake_guard = KeepAwakeGuard::new();

    logger.log(format!(
        "Stream channel established: session={} agent={} source={}",
        hello.session_id, hello.agent_name, peer
    ));

    let rdp_session = is_remote_desktop_session();
    if rdp_session {
        logger.log("Detectada sesión RDP activa. Desconectando RDP de forma automática para habilitar la captura física nativa...".to_string());
        std::thread::spawn({
            let logger = logger.clone();
            move || {
                std::thread::sleep(Duration::from_millis(500));
                if let Some(session_id) = get_current_session_id() {
                    let task_name = "LanPilotRDP";
                    
                    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
                    let schtasks_path = format!("{}\\System32\\schtasks.exe", system_root);
                    let tscon_path = format!("{}\\System32\\tscon.exe", system_root);
                    
                    let tscon_cmd = format!("{} {} /dest:console", tscon_path, session_id);
                    
                    let create_args = [
                        "/create",
                        "/tn", task_name,
                        "/tr", &tscon_cmd,
                        "/sc", "once",
                        "/sd", "01/01/2099",
                        "/st", "00:00",
                        "/ru", "NT AUTHORITY\\SYSTEM",
                        "/rl", "HIGHEST",
                        "/f"
                    ];
                    
                    let mut create_cmd = std::process::Command::new(&schtasks_path);
                    create_cmd.args(&create_args);
                    #[cfg(windows)]
                    create_cmd.creation_flags(0x08000000);
                    let _ = create_cmd.output();
                        
                    let run_args = ["/run", "/tn", task_name];
                    let mut run_cmd = std::process::Command::new(&schtasks_path);
                    run_cmd.args(&run_args);
                    #[cfg(windows)]
                    run_cmd.creation_flags(0x08000000);
                    match run_cmd.output() 
                    {
                        Ok(output) => {
                            if output.status.success() {
                                logger.log("Sesión RDP transferida exitosamente a consola vía Tarea Programada SYSTEM.".to_string());
                            } else {
                                let err_msg = String::from_utf8_lossy(&output.stderr).to_string();
                                logger.log(format!("Aviso: no se pudo transferir la sesión RDP a la consola (ID={}): {} (¿se ejecuta como Administrador?)", session_id, err_msg.trim()));
                            }
                        }
                        Err(err) => {
                            logger.log(format!("Aviso: falló la ejecución de schtasks run: {err}"));
                        }
                    }
                    
                    let delete_args = ["/delete", "/tn", task_name, "/f"];
                    let mut delete_cmd = std::process::Command::new(&schtasks_path);
                    delete_cmd.args(&delete_args);
                    #[cfg(windows)]
                    delete_cmd.creation_flags(0x08000000);
                    let _ = delete_cmd.output();
                } else {
                    logger.log("Aviso: no se pudo obtener el ID de la sesión actual de Windows".to_string());
                }
            }
        });
    }
    let recovery_probe_interval = capture_recovery_probe_interval(rdp_session);
    let mut synthetic_fallback_enabled = false;
    let capture = match stream_source {
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
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<StreamFrame>(2);
    let mut host_clipboard = arboard::Clipboard::new().ok();
    let stop_capture = stop.clone();
    let session_id_capture = hello.session_id.clone();
    let tuning_capture = Arc::clone(&tuning);
    let active_rect_capture = Arc::clone(&active_capture_rect);
    let monitor_idx_capture = Arc::clone(&active_monitor_index);
    let logger_capture = logger.clone();
    
    std::thread::spawn(move || {
        let mut capture = capture;
        let mut synthetic_fallback_enabled = synthetic_fallback_enabled;
        let mut recovery_retries = 0_u32;
        let mut compat_probe_attempts = 0_u32;
        let mut last_compat_probe = Instant::now()
            .checked_sub(CAPTURE_RECOVERY_PROBE_MIN_ELAPSED)
            .unwrap_or_else(Instant::now);
        let mut sequence = 0_u64;

        while !is_stopped(&stop_capture) {
            let tick_start = Instant::now();
            let tuning_snapshot = match tuning_capture.lock() {
                Ok(guard) => *guard,
                Err(_) => StreamTuning::default(),
            };
            let frame_interval_ms = (1000 / tuning_snapshot.target_fps.max(1)).max(16);
            let rdp_session = is_remote_desktop_session();

            if synthetic_fallback_enabled
                && stream_source == StreamSource::Screen
                && sequence > 0
                && sequence % recovery_probe_interval == 0
                && last_compat_probe.elapsed() >= CAPTURE_RECOVERY_PROBE_MIN_ELAPSED
            {
                last_compat_probe = Instant::now();
                compat_probe_attempts += 1;
                match initialize_screen_capture(&logger_capture, CAPTURE_RECOVERY_PROBE_INIT_RETRIES) {
                    Ok(new_capture) => {
                        logger_capture.log("captura de pantalla recuperada: desactivando modo de compatibilidad.".to_string());
                        capture = Some(new_capture);
                        synthetic_fallback_enabled = false;
                        recovery_retries = 0;
                        compat_probe_attempts = 0;
                    }
                    Err(err) => {
                        if compat_probe_attempts % CAPTURE_RECOVERY_PROBE_LOG_EVERY == 0 {
                            logger_capture.log(format!("captura real aún no disponible tras {compat_probe_attempts} sondeos de recuperación; se mantiene imagen de compatibilidad: {err}"));
                        }
                    }
                }
            }

            #[cfg(windows)]
            {
                let monitors = get_connected_monitors();
                if monitors.len() > 1 {
                    unsafe {
                        let mut pt = POINT { x: 0, y: 0 };
                        if GetCursorPos(&mut pt) != 0 {
                            let mut found_monitor_idx = None;
                            for (idx, m) in monitors.iter().enumerate() {
                                let right = m.0 + m.2;
                                let bottom = m.1 + m.3;
                                if pt.x >= m.0 && pt.x < right && pt.y >= m.1 && pt.y < bottom {
                                    found_monitor_idx = Some(idx);
                                    break;
                                }
                            }
                            if let Some(idx) = found_monitor_idx {
                                let current_idx = monitor_idx_capture.load(std::sync::atomic::Ordering::SeqCst);
                                if current_idx < monitors.len() && current_idx != idx {
                                    monitor_idx_capture.store(idx, std::sync::atomic::Ordering::SeqCst);
                                    let m = monitors[idx];
                                    if let Ok(mut rect_guard) = active_rect_capture.lock() {
                                        *rect_guard = m;
                                    }
                                    logger_capture.log(format!(
                                        "Ratón detectado en monitor físico {}: cambiando captura en caliente (coordenadas: left={}, top={}, width={}, height={})",
                                        idx + 1, m.0, m.1, m.2, m.3
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            let current_rect = if let Ok(guard) = active_rect_capture.lock() {
                *guard
            } else {
                let initial_w = {
                    #[cfg(windows)]
                    unsafe { GetSystemMetrics(SM_CXSCREEN) }
                    #[cfg(not(windows))]
                    1920
                };
                let initial_h = {
                    #[cfg(windows)]
                    unsafe { GetSystemMetrics(SM_CYSCREEN) }
                    #[cfg(not(windows))]
                    1080
                };
                (0, 0, initial_w, initial_h)
            };

            let mut switch_to_synthetic = false;
            let mut skip_frame = false;

            let frame = match capture.as_mut() {
                Some(screen) => match screen.capture_frame(
                    session_id_capture.clone(),
                    sequence,
                    frame_interval_ms,
                    tuning_snapshot.scale_divisor,
                    rdp_session,
                    current_rect,
                    tuning_snapshot.use_rgb565,
                ) {
                    Ok(Some(frame)) => {
                        recovery_retries = 0;
                        frame
                    }
                    Ok(None) => {
                        recovery_retries = 0;
                        skip_frame = true;
                        synthetic_frame_with_tuning(
                            session_id_capture.clone(),
                            sequence,
                            frame_interval_ms,
                            tuning_snapshot.scale_divisor,
                        )
                    }
                    Err(err) => {
                        let is_access_loss = err.contains("screen capture error") || err.contains("Access");
                        recovery_retries += 1;
                        if is_access_loss || recovery_retries == 1 {
                            logger_capture.log(format!("captura interrumpida (reintento {recovery_retries}/{CAPTURE_RECOVERY_MAX_RETRIES}): {err}"));
                        }
                        if !synthetic_fallback_enabled && should_enable_compat_fallback(&err, recovery_retries, rdp_session) {
                            logger_capture.log(if rdp_session {
                                "RDP sin imagen estable: activando imagen de compatibilidad temporal.".to_string()
                            } else {
                                "Captura de pantalla temporalmente no disponible: activando imagen de compatibilidad temporal.".to_string()
                            });
                            synthetic_fallback_enabled = true;
                            recovery_retries = 0;
                            switch_to_synthetic = true;
                            synthetic_frame_with_tuning(
                                session_id_capture.clone(),
                                sequence,
                                frame_interval_ms,
                                tuning_snapshot.scale_divisor,
                            )
                        } else {
                            match ScreenCapture::new() {
                                Ok(new_capture) => {
                                    logger_capture.log("captura reinicializada tras pérdida de acceso DXGI.".to_string());
                                    capture = Some(new_capture);
                                    recovery_retries = 0;
                                }
                                Err(reinit_err) => {
                                    logger_capture.log(format!("no se pudo reinicializar captura todavía: {reinit_err}"));
                                }
                            }
                            if recovery_retries >= CAPTURE_RECOVERY_MAX_RETRIES {
                                logger_capture.log("demasiados fallos de captura; activando compatibilidad temporal.".to_string());
                                synthetic_fallback_enabled = true;
                                recovery_retries = 0;
                                switch_to_synthetic = true;
                                synthetic_frame_with_tuning(
                                    session_id_capture.clone(),
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
                    session_id_capture.clone(),
                    sequence,
                    frame_interval_ms,
                    tuning_snapshot.scale_divisor,
                ),
            };

            if switch_to_synthetic {
                capture = None;
            }

            if !skip_frame {
                let _ = frame_tx.try_send(frame);
            }
            
            sequence += 1;
            let elapsed = tick_start.elapsed();
            let target = Duration::from_millis(frame_interval_ms as u64);
            if elapsed < target {
                thread::sleep(target - elapsed);
            }
        }
    });

    let mut last_clipboard_image_hash: Option<u64> = None;
    let mut last_clipboard_image_check = Instant::now();
    let mut last_ping_sent = Instant::now();

    while !is_stopped(&stop) {
        let now = Instant::now();

        // A. Keep-Alive Ping cada 2 segundos a través de Stream Frame
        if now.duration_since(last_ping_sent) >= Duration::from_secs(2) {
            last_ping_sent = now;
            let ping_frame = StreamFrame {
                magic: PROTOCOL_MAGIC.to_string(),
                session_id: hello.session_id.clone(),
                sequence: 0,
                captured_at_ms: unix_timestamp_ms(),
                width: 0,
                height: 0,
                stride_bytes: 0,
                pixel_format: "ping".to_string(),
                compression: StreamCompression::Lz4,
                frame_interval_ms: 1000,
                compressed_payload_b64: String::new(),
                raw_len: 0,
                source: "ping".to_string(),
                tiles: None,
            };
            if let Ok(line) = to_json_line(&ping_frame) {
                if let Err(_) = stream.write_all(line.as_bytes()) {
                    break;
                }
            }
        }

        // B. Sincronización de Imagen de Clipboard cada 1.5 segundos
        if now.duration_since(last_clipboard_image_check) >= Duration::from_millis(1500) {
            last_clipboard_image_check = now;
            if let Some(ref mut cb) = host_clipboard {
                if let Ok(img) = cb.get_image() {
                    let hash = {
                        let mut h = img.width as u64 ^ img.height as u64;
                        let len = img.bytes.len();
                        if len > 0 {
                            h = h.wrapping_add(len as u64);
                            h = h.wrapping_add(img.bytes[0] as u64);
                            h = h.wrapping_add(img.bytes[len / 2] as u64);
                            h = h.wrapping_add(img.bytes[len - 1] as u64);
                        }
                        h
                    };
                    if last_clipboard_image_hash != Some(hash) {
                        last_clipboard_image_hash = Some(hash);
                        let compressed = compress_prepend_size(&img.bytes);
                        let encoded = BASE64.encode(compressed);
                        let cb_img_frame = StreamFrame {
                            magic: PROTOCOL_MAGIC.to_string(),
                            session_id: hello.session_id.clone(),
                            sequence: 0,
                            captured_at_ms: unix_timestamp_ms(),
                            width: img.width as u32,
                            height: img.height as u32,
                            stride_bytes: img.width * 4,
                            pixel_format: "rgba".to_string(),
                            compression: StreamCompression::Lz4,
                            frame_interval_ms: 1000,
                            compressed_payload_b64: encoded,
                            raw_len: img.bytes.len(),
                            source: "clipboard_image".to_string(),
                            tiles: None,
                        };
                        if let Ok(line) = to_json_line(&cb_img_frame) {
                            if let Err(_) = stream.write_all(line.as_bytes()) {
                                break;
                            }
                        }
                    }
                }
            }
        }

        // C. Sincronización de Texto de Clipboard cada 30 ticks/segundo
        if let Some(ref mut cb) = host_clipboard {
            if let Ok(text) = cb.get_text() {
                let text_trimmed = text.trim().to_string();
                let changed = if let Ok(guard) = last_sync_clipboard.lock() {
                    !text_trimmed.is_empty() && text_trimmed != *guard
                } else {
                    false
                };
                if changed {
                    let compressed = compress_prepend_size(text_trimmed.as_bytes());
                    let encoded = BASE64.encode(compressed);
                    let cb_frame = StreamFrame {
                        magic: PROTOCOL_MAGIC.to_string(),
                        session_id: hello.session_id.clone(),
                        sequence: 0,
                        captured_at_ms: unix_timestamp_ms(),
                        width: 0,
                        height: 0,
                        stride_bytes: 0,
                        pixel_format: "text".to_string(),
                        compression: StreamCompression::Lz4,
                        frame_interval_ms: 100,
                        compressed_payload_b64: encoded,
                        raw_len: text_trimmed.len(),
                        source: "clipboard".to_string(),
                        tiles: None,
                    };
                    if let Ok(line) = to_json_line(&cb_frame) {
                        if let Err(_) = stream.write_all(line.as_bytes()) {
                            break;
                        }
                    }
                    if let Ok(mut guard) = last_sync_clipboard.lock() {
                        *guard = text_trimmed;
                    }
                }
            }
        }

        // D. Consumir y enviar frame del canal (con Frame Dropping inteligente)
        match frame_rx.recv_timeout(Duration::from_millis(15)) {
            Ok(mut frame) => {
                while let Ok(next_frame) = frame_rx.try_recv() {
                    frame = next_frame;
                }
                if let Ok(encoded) = to_json_line(&frame) {
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
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

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

#[cfg(windows)]
unsafe extern "system" fn monitor_enum_proc(
    _hmonitor: windows::Win32::Graphics::Gdi::HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    rect: *mut RECT,
    data: windows::Win32::Foundation::LPARAM,
) -> BOOL {
    unsafe {
        let rects = &mut *(data.0 as *mut Vec<(i32, i32, i32, i32)>);
        let r = *rect;
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w > 0 && h > 0 {
            rects.push((r.left, r.top, w, h));
        }
    }
    true.into()
}

#[cfg(windows)]
fn get_connected_monitors() -> Vec<(i32, i32, i32, i32)> {
    let mut rects = Vec::new();
    unsafe {
        let data = windows::Win32::Foundation::LPARAM(&mut rects as *mut _ as isize);
        let _ = windows::Win32::Graphics::Gdi::EnumDisplayMonitors(
            None,
            None,
            Some(monitor_enum_proc),
            data,
        );
    }
    rects
}

#[cfg(not(windows))]
fn get_connected_monitors() -> Vec<(i32, i32, i32, i32)> {
    vec![(0, 0, 1920, 1080)]
}

/// Screen capturer backed by GDI CreateDC("DISPLAY") — works from any thread,
/// including background service threads, and survives RDP disconnect/reconnect.
/// Unlike DXGI/WGC and xcap's GetWindowDC approach, CreateDC does not require
/// the calling thread to be attached to the interactive desktop window station.
struct ScreenCapture {
    last_bgra: Option<Vec<u8>>,
    last_frame_sent_time: std::time::Instant,
    last_tile_hashes: Vec<u64>,
    last_width: usize,
    last_height: usize,
    scaled_buf: Vec<u8>,
    rgb565_buf: Vec<u8>,
    disable_compression: bool,
    consecutive_fast_frames: usize,
}

impl ScreenCapture {
    fn new() -> Result<Self, String> {
        let initial_rect = {
            #[cfg(windows)]
            {
                unsafe {
                    (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
                }
            }
            #[cfg(not(windows))]
            (0, 0, 1920, 1080)
        };
        Self::capture_bgra(initial_rect).map(|_| Self {
            last_bgra: None,
            last_frame_sent_time: std::time::Instant::now(),
            last_tile_hashes: Vec::new(),
            last_width: 0,
            last_height: 0,
            scaled_buf: Vec::new(),
            rgb565_buf: Vec::new(),
            disable_compression: false,
            consecutive_fast_frames: 0,
        })
    }

    fn capture_frame(
        &mut self,
        session_id: String,
        sequence: u64,
        frame_interval_ms: u32,
        scale_divisor: u8,
        _detect_black_frame: bool,
        rect: (i32, i32, i32, i32),
        use_rgb565: bool,
    ) -> Result<Option<StreamFrame>, String> {
        let start_cpu = std::time::Instant::now();
        let (rect_x, rect_y, rect_w, rect_h) = rect;
        
        let (bgra, width, height) = if self.disable_compression && self.last_bgra.is_some() {
            let mut mouse_x = rect_x + rect_w / 2;
            let mut mouse_y = rect_y + rect_h / 2;
            #[cfg(windows)]
            unsafe {
                let mut pt = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut pt) != 0 {
                    mouse_x = pt.x;
                    mouse_y = pt.y;
                }
            }
            
            let focus_w = 1024_i32.min(rect_w);
            let focus_h = 768_i32.min(rect_h);
            let focus_x = (mouse_x - focus_w / 2).clamp(rect_x, rect_x + rect_w - focus_w);
            let focus_y = (mouse_y - focus_h / 2).clamp(rect_y, rect_y + rect_h - focus_h);
            let focus_rect = (focus_x, focus_y, focus_w, focus_h);
            
            let (focus_bgra, f_w, f_h) = Self::capture_bgra(focus_rect)?;
            
            if let Some(ref mut full_bgra) = self.last_bgra {
                let src_stride = f_w as usize * 4;
                let dst_stride = rect_w as usize * 4;
                for r in 0..f_h as usize {
                    let src_start = r * src_stride;
                    let src_end = src_start + src_stride;
                    let dst_start = ((focus_y - rect_y) as usize + r) * dst_stride + (focus_x - rect_x) as usize * 4;
                    if src_end <= focus_bgra.len() && dst_start + src_stride <= full_bgra.len() {
                        full_bgra[dst_start..dst_start + src_stride].copy_from_slice(&focus_bgra[src_start..src_end]);
                    }
                }
            }
            
            (self.last_bgra.clone().unwrap(), rect_w as u32, rect_h as u32)
        } else {
            let (full_bgra, w, h) = Self::capture_bgra(rect)?;
            self.last_bgra = Some(full_bgra.clone());
            (full_bgra, w, h)
        };
        
        let now = std::time::Instant::now();
        self.last_frame_sent_time = now;
        
        let stride_bytes = width as usize * 4;
        let (out_w, out_h, out_stride) =
            normalize_and_scale_bgra_in_place(&bgra, width, height, stride_bytes, scale_divisor, &mut self.scaled_buf);

        let (format_str, final_stride, bpp) = if use_rgb565 {
            let total_pixels = self.scaled_buf.len() / 4;
            self.rgb565_buf.resize(total_pixels * 2, 0);
            
            let src = &self.scaled_buf;
            unsafe {
                let dst_u16 = std::slice::from_raw_parts_mut(self.rgb565_buf.as_mut_ptr() as *mut u16, total_pixels);
                for i in 0..total_pixels {
                    let offset = i * 4;
                    let b = src[offset] as u16;
                    let g = src[offset + 1] as u16;
                    let r = src[offset + 2] as u16;
                    let r5 = r >> 3;
                    let g6 = g >> 2;
                    let b5 = b >> 3;
                    dst_u16[i] = (r5 << 11) | (g6 << 5) | b5;
                }
            }
            ("rgb565".to_string(), out_w as usize * 2, 2)
        } else {
            ("bgra8".to_string(), out_stride, 4)
        };

        let final_payload = if use_rgb565 { &self.rgb565_buf } else { &self.scaled_buf };

        let cols = (out_w as usize + 63) / 64;
        let rows = (out_h as usize + 63) / 64;
        let total_tiles = cols * rows;
        let mut force_full_frame = false;

        if self.last_width != out_w as usize || self.last_height != out_h as usize || self.last_tile_hashes.len() != total_tiles {
            self.last_tile_hashes = vec![0; total_tiles];
            self.last_width = out_w as usize;
            self.last_height = out_h as usize;
            force_full_frame = true;
        }

        let mut dirty_tiles = Vec::new();
        let mut tile_bytes = Vec::with_capacity(64 * 64 * 4);

        struct TileJob {
            x: u16,
            y: u16,
            width: u16,
            height: u16,
            tile_bytes: Vec<u8>,
            idx: usize,
            hash: u64,
        }

        let mut jobs = Vec::new();

        for row in 0..rows {
            for col in 0..cols {
                let x = col * 64;
                let y = row * 64;
                let tile_w = std::cmp::min(64, out_w as usize - x);
                let tile_h = std::cmp::min(64, out_h as usize - y);

                tile_bytes.clear();
                for r in 0..tile_h {
                    let row_offset = (y + r) * final_stride;
                    let col_offset = x * bpp;
                    let start = row_offset + col_offset;
                    let end = start + tile_w * bpp;
                    if end <= final_payload.len() {
                        tile_bytes.extend_from_slice(&final_payload[start..end]);
                    }
                }

                let mut hash = 2166136261_u64;
                for &b in &tile_bytes {
                    hash = hash ^ (b as u64);
                    hash = hash.wrapping_mul(16777619);
                }

                let idx = row * cols + col;
                if force_full_frame || self.last_tile_hashes[idx] != hash {
                    jobs.push(TileJob {
                        x: x as u16,
                        y: y as u16,
                        width: tile_w as u16,
                        height: tile_h as u16,
                        tile_bytes: tile_bytes.clone(),
                        idx,
                        hash,
                    });
                }
            }
        }

        if !jobs.is_empty() {
            let num_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            let chunk_size = (jobs.len() + num_cpus - 1) / num_cpus;
            let results_mutex = std::sync::Mutex::new(Vec::new());

            std::thread::scope(|s| {
                for chunk in jobs.chunks_mut(chunk_size) {
                    s.spawn(|| {
                        let mut local_results = Vec::new();
                        for job in chunk {
                            let compressed = compress_prepend_size(&job.tile_bytes);
                            let encoded = BASE64.encode(compressed);
                            local_results.push((job.idx, job.hash, lanpilot_core::Tile {
                                x: job.x,
                                y: job.y,
                                width: job.width,
                                height: job.height,
                                compressed_payload_b64: encoded,
                                raw_len: job.tile_bytes.len(),
                            }));
                        }
                        if let Ok(mut guard) = results_mutex.lock() {
                            guard.extend(local_results);
                        }
                    });
                }
            });

            if let Ok(results) = results_mutex.into_inner() {
                for (idx, hash, tile) in results {
                    self.last_tile_hashes[idx] = hash;
                    dirty_tiles.push(tile);
                }
            }
        }

        if dirty_tiles.is_empty() && !force_full_frame {
            return Ok(None);
        }

        let (tiles_payload, frame_payload_b64, frame_raw_len, compression_type) = if self.disable_compression {
            let encoded = BASE64.encode(final_payload);
            (None, encoded, final_payload.len(), StreamCompression::None)
        } else {
            let use_tiles = !force_full_frame && (dirty_tiles.len() <= (total_tiles * 80 / 100));
            if use_tiles {
                (Some(dirty_tiles), "".to_string(), 0, StreamCompression::Lz4)
            } else {
                let compressed = compress_prepend_size(final_payload);
                let encoded = BASE64.encode(compressed);
                (None, encoded, final_payload.len(), StreamCompression::Lz4)
            }
        };

        let cpu_duration = start_cpu.elapsed();
        if cpu_duration > std::time::Duration::from_millis(12) {
            if !self.disable_compression {
                self.disable_compression = true;
                self.consecutive_fast_frames = 0;
            }
        } else {
            if self.disable_compression {
                self.consecutive_fast_frames += 1;
                if self.consecutive_fast_frames >= 30 {
                    self.disable_compression = false;
                    self.consecutive_fast_frames = 0;
                }
            }
        }

        Ok(Some(StreamFrame {
            magic: PROTOCOL_MAGIC.to_string(),
            session_id,
            sequence,
            captured_at_ms: unix_timestamp_ms(),
            width: out_w,
            height: out_h,
            stride_bytes: final_stride,
            pixel_format: format_str,
            compression: compression_type,
            frame_interval_ms,
            compressed_payload_b64: frame_payload_b64,
            raw_len: frame_raw_len,
            source: "screen".to_string(),
            tiles: tiles_payload,
        }))
    }

    /// Capture the primary monitor using GDI CreateDC("DISPLAY").
    /// Returns raw BGRA pixels (alpha = 255) with (width, height).
    #[cfg(windows)]
    fn capture_bgra(rect: (i32, i32, i32, i32)) -> Result<(Vec<u8>, u32, u32), String> {
        unsafe {
            let x = rect.0;
            let y = rect.1;
            let width = rect.2;
            let height = rect.3;
            if width <= 0 || height <= 0 {
                return Err(format!(
                    "screen capture error: invalid dimensions {width}x{height}"
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

            BitBlt(hdc_mem, 0, 0, width, height, Some(hdc_screen), x, y, SRCCOPY)
                .map_err(|e| format!("screen capture error: BitBlt failed: {e}"))?;

            // Dibujar el cursor de ratón en el hdc_mem para integrarlo en el stream de video
            let mut cursor_info = CURSORINFO {
                cb_size: std::mem::size_of::<CURSORINFO>() as u32,
                flags: 0,
                h_cursor: std::ptr::null_mut(),
                pt_screen_pos: POINT { x: 0, y: 0 },
            };
            if GetCursorInfo(&mut cursor_info) != 0 && cursor_info.flags == 1 {
                let mut icon_info = ICONINFO {
                    f_icon: 0,
                    x_hotspot: 0,
                    y_hotspot: 0,
                    hbm_mask: std::ptr::null_mut(),
                    hbm_color: std::ptr::null_mut(),
                };
                if GetIconInfo(cursor_info.h_cursor, &mut icon_info) != 0 {
                    let cursor_x = cursor_info.pt_screen_pos.x - icon_info.x_hotspot as i32;
                    let cursor_y = cursor_info.pt_screen_pos.y - icon_info.y_hotspot as i32;
                    
                    DrawIconEx(
                        hdc_mem.0 as _,
                        cursor_x,
                        cursor_y,
                        cursor_info.h_cursor,
                        0,
                        0,
                        0,
                        std::ptr::null_mut(),
                        0x0003, // DI_NORMAL = 3
                    );
                    
                    if !icon_info.hbm_mask.is_null() {
                        let _ = DeleteObject(windows::Win32::Graphics::Gdi::HBITMAP(icon_info.hbm_mask as _).into());
                    }
                    if !icon_info.hbm_color.is_null() {
                        let _ = DeleteObject(windows::Win32::Graphics::Gdi::HBITMAP(icon_info.hbm_color as _).into());
                    }
                }
            }

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
    fn capture_bgra(_rect: (i32, i32, i32, i32)) -> Result<(Vec<u8>, u32, u32), String> {
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



#[allow(dead_code)]
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

fn normalize_and_scale_bgra_in_place(
    input: &[u8],
    width: u32,
    height: u32,
    input_stride: usize,
    scale_divisor: u8,
    out: &mut Vec<u8>,
) -> (u32, u32, usize) {
    let divisor = scale_divisor.clamp(1, 3) as usize;
    let out_width = (width as usize / divisor).max(1);
    let out_height = (height as usize / divisor).max(1);
    let out_stride = out_width * 4;
    
    let required_len = out_height * out_stride;
    if out.len() != required_len {
        out.resize(required_len, 0);
    }
    
    if divisor == 1 && input_stride == out_stride {
        out.copy_from_slice(input);
        return (out_width as u32, out_height as u32, out_stride);
    }
    
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

    (out_width as u32, out_height as u32, out_stride)
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
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
            fn GetCurrentProcessId() -> u32;
            fn WTSGetActiveConsoleSessionId() -> u32;
        }
        unsafe {
            let pid = GetCurrentProcessId();
            let mut session_id = 0u32;
            if ProcessIdToSessionId(pid, &mut session_id) != 0 {
                let console_id = WTSGetActiveConsoleSessionId();
                session_id != console_id && session_id != 0
            } else {
                false
            }
        }
    }
    #[cfg(not(windows))]
    false
}

#[allow(dead_code)]
fn is_remote_desktop_session_name(session_name: &str) -> bool {
    session_name
        .trim()
        .to_ascii_lowercase()
        .starts_with("rdp-")
}

fn get_current_session_id() -> Option<u32> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
            fn GetCurrentProcessId() -> u32;
        }
        unsafe {
            let pid = GetCurrentProcessId();
            let mut session_id = 0u32;
            if ProcessIdToSessionId(pid, &mut session_id) != 0 {
                Some(session_id)
            } else {
                None
            }
        }
    }
    #[cfg(not(windows))]
    None
}

fn run_audio_server(
    listener: std::net::TcpListener,
    logger: Logger,
    stop: StopFlag,
) {
    logger.log("Servidor de audio iniciado. Esperando conexión del agente...".to_string());
    let _ = listener.set_nonblocking(true);
    
    while !is_stopped(&stop) {
        match listener.accept() {
            Ok((mut stream, addr)) => {
                let _ = stream.set_nodelay(true);
                logger.log(format!("Agente conectado para audio desde {}", addr));
                let logger = logger.clone();
                let stop = Arc::clone(&stop);
                
                std::thread::spawn(move || {
                    if let Err(err) = handle_audio_stream(&mut stream, &logger, stop) {
                        logger.log(format!("Error en sesión de audio: {}", err));
                    }
                    logger.log("Sesión de audio finalizada.".to_string());
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(err) => {
                logger.log(format!("Error en listener de audio accept: {}", err));
                break;
            }
        }
    }
}

fn handle_audio_stream(
    stream: &mut std::net::TcpStream,
    logger: &Logger,
    stop: StopFlag,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::io::Write;
    use std::time::Duration;
    
    #[cfg(windows)]
    unsafe {
        #[link(name = "ole32")]
        unsafe extern "system" {
            fn CoInitializeEx(pv_reserved: *mut std::ffi::c_void, dw_co_init: u32) -> i32;
        }
        // COINIT_APARTMENTTHREADED = 0x2
        let _ = CoInitializeEx(std::ptr::null_mut(), 0x2);
    }
    
    let host = cpal::default_host();
    let mut device = None;
    let mut config = None;
    
    for i in 0..5 {
        if is_stopped(&stop) {
            return Err("Detenido por el usuario".to_string());
        }
        if let Some(d) = host.default_output_device() {
            if let Ok(c) = d.default_output_config() {
                device = Some(d);
                config = Some(c);
                break;
            }
        }
        logger.log(format!("Esperando a que la tarjeta de sonido física se reactive tras desconexión RDP (intento {}/5)...", i + 1));
        std::thread::sleep(Duration::from_millis(1000));
    }
    
    let device = device.ok_or_else(|| "No se encontró ningún dispositivo de salida de audio activo en el sistema".to_string())?;
    let config = config.ok_or_else(|| "No se pudo obtener la configuración del dispositivo de audio".to_string())?;
        
    let stream_config: cpal::StreamConfig = config.clone().into();
    let sample_rate = stream_config.sample_rate.0;
    let channels = stream_config.channels;
    
    logger.log(format!(
        "Capturando audio Loopback: {} Hz, {} canales, formato {:?}",
        sample_rate, channels, config.sample_format()
    ));
    
    let mut header = [0u8; 8];
    header[0..4].copy_from_slice(&sample_rate.to_le_bytes());
    header[4..6].copy_from_slice(&channels.to_le_bytes());
    header[6..8].copy_from_slice(&1_u16.to_le_bytes()); // Audio LZ4 compression enabled flag
    stream
        .write_all(&header)
        .map_err(|e| format!("Error al escribir cabecera de audio: {e}"))?;
        
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<i16>>(20);
    let err_fn = |err| eprintln!("an error occurred on stream: {}", err);
    
    let cpal_stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let samples: Vec<i16> = data
                        .iter()
                        .map(|&s| {
                            let scaled = s * 32767.0;
                            scaled.clamp(-32768.0, 32767.0) as i16
                        })
                        .collect();
                    let _ = tx.try_send(samples);
                },
                err_fn,
                None
            )
        }
        cpal::SampleFormat::I16 => {
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let _ = tx.try_send(data.to_vec());
                },
                err_fn,
                None
            )
        }
        cpal::SampleFormat::U16 => {
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let samples: Vec<i16> = data
                        .iter()
                        .map(|&s| (s as i32 - 32768) as i16)
                        .collect();
                    let _ = tx.try_send(samples);
                },
                err_fn,
                None
            )
        }
        _ => return Err("Formato de audio no soportado".to_string()),
    }.map_err(|e| format!("Error al construir stream de captura de audio: {e}"))?;
    
    cpal_stream
        .play()
        .map_err(|e| format!("Error al iniciar stream de captura de audio: {e}"))?;
        
    let _ = stream.set_nonblocking(false);
    let mut byte_buffer = Vec::new();
    let mut predictor = 0;
    let mut step_index = 0;
    
    while !is_stopped(&stop) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(samples) => {
                byte_buffer.clear();
                let mut i = 0;
                while i < samples.len() {
                    let s1 = samples[i];
                    let code1 = lanpilot_core::adpcm_encode_sample(s1, &mut predictor, &mut step_index);
                    let code2 = if i + 1 < samples.len() {
                        let s2 = samples[i + 1];
                        lanpilot_core::adpcm_encode_sample(s2, &mut predictor, &mut step_index)
                    } else {
                        0
                    };
                    let byte = (code2 << 4) | (code1 & 0x0F);
                    byte_buffer.push(byte);
                    i += 2;
                }
                let compressed = lz4_flex::compress_prepend_size(&byte_buffer);
                let len = compressed.len() as u32;
                let mut write_err = stream.write_all(&len.to_le_bytes());
                if write_err.is_ok() {
                    write_err = stream.write_all(&compressed);
                }
                if let Err(e) = write_err {
                    logger.log(format!("Desconexión de transmisión de audio comprimido: {e}"));
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
    
    let _ = cpal_stream.pause();
    Ok(())
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
        let test_ip = local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        let blocker = bind_udp(test_ip, DISCOVERY_PORT);
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

#[cfg(windows)]
#[repr(C)]
struct POINT {
    x: i32,
    y: i32,
}

#[cfg(windows)]
#[repr(C)]
struct CURSORINFO {
    cb_size: u32,
    flags: u32,
    h_cursor: *mut std::ffi::c_void,
    pt_screen_pos: POINT,
}

#[cfg(windows)]
#[repr(C)]
struct ICONINFO {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: *mut std::ffi::c_void,
    hbm_color: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn SetCursorPos(x: i32, y: i32) -> i32;

    fn BlockInput(fBlockIt: i32) -> i32;
    fn SendNotifyMessageW(hWnd: *mut std::ffi::c_void, Msg: u32, wParam: usize, lParam: isize) -> i32;
    fn GetCursorInfo(pci: *mut CURSORINFO) -> i32;
    fn GetIconInfo(hIcon: *mut std::ffi::c_void, piconinfo: *mut ICONINFO) -> i32;
    fn DrawIconEx(
        hdc: *mut std::ffi::c_void,
        xLeft: i32,
        yTop: i32,
        hIcon: *mut std::ffi::c_void,
        cxWidth: i32,
        cyWidth: i32,
        istepIfAniCur: u32,
        hbrFlickerFreeDraw: *mut std::ffi::c_void,
        diFlags: u32,
    ) -> i32;
    fn GetKeyState(n_virt_key: i32) -> i16;
    fn GetCursorPos(lpPoint: *mut POINT) -> i32;
    fn SendInput(cInputs: u32, pInputs: *const INPUT, cbSize: i32) -> u32;
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
struct KEYBDINPUT {
    wVk: u16,
    wScan: u16,
    dwFlags: u32,
    time: u32,
    dwExtraInfo: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
struct MOUSEINPUT {
    dx: i32,
    dy: i32,
    mouseData: u32,
    dwFlags: u32,
    time: u32,
    dwExtraInfo: usize,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
struct HARDWAREINPUT {
    uMsg: u32,
    wParamL: u16,
    wParamH: u16,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
union INPUT_UNION {
    mi: MOUSEINPUT,
    ki: KEYBDINPUT,
    hi: HARDWAREINPUT,
}

#[cfg(windows)]
#[repr(C)]
struct INPUT {
    r#type: u32,
    u: INPUT_UNION,
}

#[cfg(windows)]
fn send_keyboard_input(vk: u16, scan: u16, flags: u32) {
    unsafe {
        let input = INPUT {
            r#type: 1, // INPUT_KEYBOARD
            u: INPUT_UNION {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                }
            }
        };
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(windows)]
fn send_mouse_input(dw_flags: u32, dx: i32, dy: i32, dw_data: u32) {
    unsafe {
        let mut input = INPUT {
            r#type: 0, // INPUT_MOUSE = 0
            u: std::mem::zeroed(),
        };
        input.u.mi = MOUSEINPUT {
            dx,
            dy,
            mouseData: dw_data,
            dwFlags: dw_flags,
            time: 0,
            dwExtraInfo: 0,
        };
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(not(windows))]
fn send_keyboard_input(_vk: u16, _scan: u16, _flags: u32) {}

#[cfg(not(windows))]
fn send_mouse_input(_dw_flags: u32, _dx: i32, _dy: i32, _dw_data: u32) {}

#[cfg(windows)]
fn map_key_to_vk(key: &str) -> Option<u8> {
    if key.len() == 1 {
        let c = key.chars().next().unwrap().to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Some(c as u8);
        }
    }
    if key.starts_with("Key") && key.len() == 4 {
        let c = key.chars().nth(3).unwrap();
        if c.is_ascii_digit() {
            return Some(c as u8);
        }
    }
    if key.starts_with("NumPad") && key.len() == 7 {
        let c = key.chars().nth(6).unwrap();
        if c.is_ascii_digit() {
            let digit = c.to_digit(10).unwrap() as u8;
            return Some(0x60 + digit);
        }
    }
    match key {
        "Enter" | "Return" => Some(0x0D), // VK_RETURN
        "Backspace" => Some(0x08), // VK_BACK
        "Tab" => Some(0x09), // VK_TAB
        "Space" | " " => Some(0x20), // VK_SPACE
        "Escape" => Some(0x1B), // VK_ESCAPE
        "LeftCtrl" | "RightCtrl" | "LControl" | "RControl" | "Control" => Some(0x11), // VK_CONTROL
        "LeftShift" | "RightShift" | "LShift" | "RShift" | "Shift" => Some(0x10), // VK_SHIFT
        "LeftAlt" | "RightAlt" | "LAlt" | "RAlt" | "Alt" => Some(0x12), // VK_MENU
        "Left" => Some(0x25), // VK_LEFT
        "Up" => Some(0x26), // VK_UP
        "Right" => Some(0x27), // VK_RIGHT
        "Down" => Some(0x28), // VK_DOWN
        "LWin" | "RWin" | "Win" | "Meta" => Some(0x5B), // VK_LWIN
        "Delete" => Some(0x2E), // VK_DELETE
        "Insert" => Some(0x2D), // VK_INSERT
        "Home" => Some(0x24), // VK_HOME
        "End" => Some(0x23), // VK_END
        "PageUp" => Some(0x21), // VK_PRIOR (Page Up)
        "PageDown" => Some(0x22), // VK_NEXT (Page Down)
        "F1" => Some(0x70),
        "F2" => Some(0x71),
        "F3" => Some(0x72),
        "F4" => Some(0x73),
        "F5" => Some(0x74),
        "F6" => Some(0x75),
        "F7" => Some(0x76),
        "F8" => Some(0x77),
        "F9" => Some(0x78),
        "F10" => Some(0x79),
        "F11" => Some(0x7A),
        "F12" => Some(0x7B),
        _ => None,
    }
}
