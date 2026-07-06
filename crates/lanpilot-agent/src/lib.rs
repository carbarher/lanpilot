//! LanPilot agent runtime library.
//!
//! This crate exposes [`run_agent`], a blocking entrypoint that runs the
//! full LanPilot agent connection flow (discovery, handshake, remote-input
//! edge switch, and screen-stream rendering) in-process. It backs both the
//! thin `lanpilot-agent` CLI binary and `lanpilot-app`, the GUI wrapper,
//! which drives it from a background thread.
//!
//! Design notes mirror `lanpilot-host`:
//! - Configuration is explicit via [`AgentConfig`] — no environment
//!   variables or stdin prompts inside this crate.
//! - Status lines go through a [`Logger`] callback instead of `println!`.
//! - Cancellation is cooperative via a [`StopFlag`]: the connect/retry loops
//!   check it between steps so a GUI Cancel button can interrupt an
//!   in-progress connection attempt without killing the process.
//! - No `process::exit` — every failure is returned as `Err(String)`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lanpilot_core::{
    ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse, EdgeDirection,
    EdgeSwitchConfig, HandshakeAck, HandshakeHello, Logger, PRODUCT_NAME, PROTOCOL_MAGIC, StopFlag,
    SessionEvent, StreamCompression, StreamFrame, StreamHello, TAGLINE, from_json_line, is_stopped,
    log_session_event, normalize_pair_code, should_switch_to_remote, to_json_line, unix_timestamp_ms,
};
use lz4_flex::decompress_size_prepended;
use minifb::{Scale, Window, WindowOptions};

/// Explicit configuration for [`run_agent`]. No environment variables or
/// stdin prompts are read by this crate — the CLI wrapper resolves those and
/// populates this struct; the GUI app supplies its own values directly.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Friendly agent name announced to the host. Defaults to `"lanpilot-agent"`.
    pub agent_name: Option<String>,
    /// Six-digit pairing code. Must already be normalized (exactly 6 digits).
    pub pair_code: String,
    /// Whether to open a `minifb` window to render incoming stream frames.
    pub render_enabled: bool,
    /// Number of stream frames to receive before the session ends.
    /// Use `0` to keep the stream active until the user stops it.
    pub target_stream_frames: u32,
    /// Optional preferred host IP discovered by the caller. If set, discovery
    /// keeps listening until this host appears or times out.
    pub preferred_host_ipv4: Option<String>,
    /// Optional preferred host machine name. Used as fallback when preferred
    /// IP changed (e.g. DHCP) but the host keeps the same PC name.
    pub preferred_host_name: Option<String>,
}

impl AgentConfig {
    /// Build a config with a given pair code and otherwise-default settings.
    pub fn with_pair_code(pair_code: impl Into<String>) -> Self {
        Self {
            agent_name: None,
            pair_code: pair_code.into(),
            render_enabled: true,
            target_stream_frames: 60,
            preferred_host_ipv4: None,
            preferred_host_name: None,
        }
    }
}

/// Run the LanPilot agent connection flow to completion (or until `stop` is
/// set / an unrecoverable error occurs).
///
/// This function blocks the calling thread; run it on a background thread
/// (as `lanpilot-app` does) if the caller also needs to keep servicing a UI.
pub fn run_agent(config: AgentConfig, logger: Logger, stop: StopFlag) -> Result<(), String> {
    let agent_name = config.agent_name.clone().unwrap_or_else(|| "lanpilot-agent".to_string());
    let pair_code = normalize_pair_code(&config.pair_code)
        .ok_or_else(|| "el código de emparejamiento debe tener exactamente 6 dígitos".to_string())?;

    logger.log(format!("{PRODUCT_NAME} Agent"));
    logger.log(TAGLINE.to_string());
    logger.log("=== Conectarme ===".to_string());
    logger.log(format!("Enviando sonda de descubrimiento en UDP {DISCOVERY_PORT}..."));

    if is_stopped(&stop) {
        return Err("cancelado por el usuario".to_string());
    }
    let connect_started = Instant::now();
    let candidates = resolve_host_candidates(
        &agent_name,
        &pair_code,
        config.preferred_host_ipv4.as_deref(),
        config.preferred_host_name.as_deref(),
        &logger,
    )?;
    let candidates = prioritize_host_candidates_by_probe(candidates, &logger);
    let total_candidates = candidates.len();
    let mut last_error: Option<String> = None;

    for (index, discovered) in candidates.into_iter().enumerate() {
        if is_stopped(&stop) {
            return Err("cancelado por el usuario".to_string());
        }
        logger.log(format!(
            "Intentando equipo {}/{}: {} ({}:{})",
            index + 1,
            total_candidates,
            discovered.host_name,
            discovered.host_ipv4,
            discovered.handshake_port
        ));
        if let Some(preferred_ip) = config.preferred_host_ipv4.as_deref() {
            if preferred_ip != discovered.host_ipv4 {
                logger.log(format!(
                    "El host cambió de IP (antes {preferred_ip}, ahora {}). Se continuó por nombre de equipo.",
                    discovered.host_ipv4
                ));
            }
        }

        let handshake_started = Instant::now();
        let ack = match perform_handshake(&agent_name, &discovered) {
            Ok(ack) => ack,
            Err(err) if is_transient_message(&err) => {
                last_error = Some(err.clone());
                logger.log(format!(
                    "[RECONNECT] no se pudo conectar con {} ({}), motivo: {err}. Probando otro equipo...",
                    discovered.host_name, discovered.host_ipv4,
                ));
                continue;
            }
            Err(err) => {
                last_error = Some(err.clone());
                logger.log(format!(
                    "[RECONNECT] error al conectar con {} ({}): {err}",
                    discovered.host_name, discovered.host_ipv4
                ));
                continue;
            }
        };
        logger.log(format!(
            "[METRIC] handshake_ms={}",
            handshake_started.elapsed().as_millis()
        ));
        logger.log(format!(
            "[METRIC] connect_total_ms={}",
            connect_started.elapsed().as_millis()
        ));
        logger.log(format!("✓ Conectado a {}. Sesión activa.", ack.host_name));

        match run_phase2_with_retry(&discovered.host_ipv4, &ack, &logger, &stop) {
            Ok(()) => {}
            Err(err) if is_transient_message(&err) => {
                last_error = Some(err);
                logger.log("[RECONNECT] phase 2 falló, probando otro equipo...".to_string());
                continue;
            }
            Err(err) => return Err(err),
        }
        match run_phase3_with_retry(&discovered.host_ipv4, &agent_name, &ack, &logger, &stop, &config) {
            Ok(()) => return Ok(()),
            Err(err) if is_transient_message(&err) => {
                last_error = Some(err);
                logger.log("[RECONNECT] phase 3 falló, probando otro equipo...".to_string());
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        "No se pudo completar la conexión con los equipos disponibles.".to_string()
    }))
}

pub fn discover_hosts(pair_code: &str) -> Result<Vec<DiscoveryResponse>, String> {
    let normalized = normalize_pair_code(pair_code)
        .ok_or_else(|| "el código de emparejamiento debe tener exactamente 6 dígitos".to_string())?;
    discover_hosts_with_timeout("lanpilot-app", &normalized, Duration::from_millis(1_200))
}

fn resolve_host_candidates(
    agent_name: &str,
    pair_code: &str,
    preferred_host_ipv4: Option<&str>,
    preferred_host_name: Option<&str>,
    logger: &Logger,
) -> Result<Vec<DiscoveryResponse>, String> {
    let discovery_started = Instant::now();
    let responses = discover_hosts_with_timeout(agent_name, pair_code, Duration::from_millis(1_600))?;
    logger.log(format!(
        "[METRIC] discovery_ms={} candidates={}",
        discovery_started.elapsed().as_millis(),
        responses.len()
    ));
    let ordered = order_host_candidates(responses, preferred_host_ipv4, preferred_host_name);
    if ordered.is_empty() {
        if let Some(preferred_name) = preferred_host_name {
            return Err(format!(
                "No se encontró el equipo seleccionado ({preferred_name}) en la red local."
            ));
        }
        if let Some(preferred_ip) = preferred_host_ipv4 {
            return Err(format!(
                "No se encontró el equipo seleccionado ({preferred_ip}) en la red local."
            ));
        }
        return Err("No se encontró ningún host LanPilot en la red local.".to_string());
    }
    Ok(ordered)
}

fn order_host_candidates(
    responses: Vec<DiscoveryResponse>,
    preferred_host_ipv4: Option<&str>,
    preferred_host_name: Option<&str>,
) -> Vec<DiscoveryResponse> {
    if responses.is_empty() {
        return responses;
    }

    let mut exact_ip = Vec::new();
    let mut same_name = Vec::new();
    let mut others = Vec::new();

    for response in responses {
        let ip_match = preferred_host_ipv4
            .map(|ip| response.host_ipv4 == ip)
            .unwrap_or(false);
        let name_match = preferred_host_name
            .map(|name| response.host_name.eq_ignore_ascii_case(name))
            .unwrap_or(false);

        if ip_match {
            exact_ip.push(response);
        } else if name_match {
            same_name.push(response);
        } else {
            others.push(response);
        }
    }

    exact_ip.sort_by(|a, b| a.host_ipv4.cmp(&b.host_ipv4));
    same_name.sort_by(|a, b| a.host_ipv4.cmp(&b.host_ipv4));
    others.sort_by(|a, b| a.host_name.cmp(&b.host_name).then(a.host_ipv4.cmp(&b.host_ipv4)));

    match (preferred_host_ipv4, preferred_host_name) {
        (Some(_), Some(_)) => {
            if exact_ip.is_empty() && same_name.is_empty() {
                return Vec::new();
            }
            exact_ip.extend(same_name);
            exact_ip.extend(others);
            exact_ip
        }
        (Some(_), None) => {
            if exact_ip.is_empty() {
                return Vec::new();
            }
            exact_ip.extend(others);
            exact_ip
        }
        (None, Some(_)) => {
            if same_name.is_empty() {
                return Vec::new();
            }
            same_name.extend(others);
            same_name
        }
        (None, None) => {
            others.extend(exact_ip);
            others.extend(same_name);
            others.sort_by(|a, b| a.host_name.cmp(&b.host_name).then(a.host_ipv4.cmp(&b.host_ipv4)));
            others
        }
    }
}

fn discover_hosts_with_timeout(
    agent_name: &str,
    pair_code: &str,
    timeout: Duration,
) -> Result<Vec<DiscoveryResponse>, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|err| format!("bind discovery socket failed: {err}"))?;
    socket
        .set_broadcast(true)
        .map_err(|err| format!("set broadcast failed: {err}"))?;
    let read_slice = std::cmp::min(timeout, Duration::from_millis(220));
    socket
        .set_read_timeout(Some(read_slice))
        .map_err(|err| format!("set read timeout failed: {err}"))?;

    let probe = DiscoveryProbe::new(agent_name.to_string(), pair_code.to_string());
    let payload = to_json_line(&probe).map_err(|err| format!("encode probe failed: {err}"))?;
    let target = SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT));
    socket
        .send_to(payload.as_bytes(), target)
        .map_err(|err| format!("send discovery failed: {err}"))?;

    let started = Instant::now();
    let mut rebroadcasted = false;
    let mut by_ip: HashMap<String, DiscoveryResponse> = HashMap::new();
    let mut buf = [0_u8; 2048];

    while started.elapsed() < timeout {
        if !rebroadcasted && started.elapsed() >= timeout / 2 {
            socket
                .send_to(payload.as_bytes(), target)
                .map_err(|err| format!("resend discovery failed: {err}"))?;
            rebroadcasted = true;
        }
        match socket.recv_from(&mut buf) {
            Ok((received, source)) => {
                let response_line = std::str::from_utf8(&buf[..received])
                    .map_err(|err| format!("utf8 discovery response failed: {err}"))?;
                let response: DiscoveryResponse = from_json_line(response_line)
                    .map_err(|err| format!("decode discovery response failed: {err}"))?;

                if response.magic != PROTOCOL_MAGIC {
                    continue;
                }

                // Reject forged responses: the IP in the payload must match the packet source.
                let claimed_ip = response
                    .host_ipv4
                    .parse::<std::net::IpAddr>()
                    .map_err(|err| format!("invalid host_ipv4 in discovery response: {err}"))?;
                if source.ip() != claimed_ip {
                    continue;
                }

                by_ip.entry(response.host_ipv4.clone()).or_insert(response);
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(err) => return Err(format!("receive discovery response failed: {err}")),
        }
    }

    let mut responses: Vec<DiscoveryResponse> = by_ip.into_values().collect();
    responses.sort_by(|a, b| a.host_name.cmp(&b.host_name).then(a.host_ipv4.cmp(&b.host_ipv4)));
    if responses.is_empty() {
        return Err(
            "No se encontró ningún host LanPilot en la red local. Asegúrate de que el equipo remoto esté ejecutándose y de que ambos equipos estén en la misma red."
                .to_string(),
        );
    }
    Ok(responses)
}

const HANDSHAKE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1_200);

#[derive(Clone, Debug, PartialEq, Eq)]
struct HandshakeProbeResult {
    index: usize,
    reachable: bool,
    latency_ms: u128,
}

fn prioritize_host_candidates_by_probe(
    candidates: Vec<DiscoveryResponse>,
    logger: &Logger,
) -> Vec<DiscoveryResponse> {
    if candidates.len() <= 1 {
        return candidates;
    }

    let probe_timeout = handshake_probe_timeout(candidates.len());
    let collect_timeout = probe_timeout + Duration::from_millis(260);
    let probe_started = Instant::now();
    let (tx, rx) = mpsc::channel::<HandshakeProbeResult>();
    let total = candidates.len();
    for (index, candidate) in candidates.iter().cloned().enumerate() {
        let tx = tx.clone();
        let probe_timeout = probe_timeout;
        thread::spawn(move || {
            let started = Instant::now();
            let reachable = socket_addr_for_host(&candidate)
                .ok()
                .and_then(|socket_addr| {
                    TcpStream::connect_timeout(&socket_addr, probe_timeout).ok()
                })
                .is_some();
            let _ = tx.send(HandshakeProbeResult {
                index,
                reachable,
                latency_ms: started.elapsed().as_millis(),
            });
        });
    }
    drop(tx);

    let mut results = Vec::with_capacity(total);
    for _ in 0..total {
        match rx.recv_timeout(collect_timeout) {
            Ok(result) => results.push(result),
            Err(_) => break,
        }
    }

    let reachable_count = results.iter().filter(|result| result.reachable).count();
    if reachable_count > 0 {
        logger.log(format!(
            "Sondeo rápido: {reachable_count}/{total} equipos con puerto de conexión activo."
        ));
    }
    logger.log(format!(
        "[METRIC] probe_ms={} reachable_candidates={} total_candidates={}",
        probe_started.elapsed().as_millis(),
        reachable_count,
        total
    ));

    apply_probe_priority(candidates, &results)
}

fn handshake_probe_timeout(total_candidates: usize) -> Duration {
    if total_candidates >= 12 {
        Duration::from_millis(240)
    } else if total_candidates >= 6 {
        Duration::from_millis(320)
    } else {
        Duration::from_millis(450)
    }
}

fn apply_probe_priority(
    candidates: Vec<DiscoveryResponse>,
    probe_results: &[HandshakeProbeResult],
) -> Vec<DiscoveryResponse> {
    let mut by_index: HashMap<usize, (bool, u128)> = HashMap::new();
    for result in probe_results {
        by_index.insert(result.index, (result.reachable, result.latency_ms));
    }

    let mut reachable = Vec::new();
    let mut remaining = Vec::new();
    for (index, candidate) in candidates.into_iter().enumerate() {
        match by_index.get(&index) {
            Some((true, latency_ms)) => reachable.push((index, *latency_ms, candidate)),
            _ => remaining.push((index, candidate)),
        }
    }

    reachable.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    remaining.sort_by(|a, b| a.0.cmp(&b.0));

    reachable
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .chain(remaining.into_iter().map(|(_, candidate)| candidate))
        .collect()
}

fn socket_addr_for_host(discovered: &DiscoveryResponse) -> Result<SocketAddr, String> {
    format!("{}:{}", discovered.host_ipv4, discovered.handshake_port)
        .parse()
        .map_err(|err| format!("invalid host endpoint: {err}"))
}

fn perform_handshake(
    agent_name: &str,
    discovered: &DiscoveryResponse,
) -> Result<HandshakeAck, String> {
    let socket_addr = socket_addr_for_host(discovered)?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, HANDSHAKE_CONNECT_TIMEOUT)
        .map_err(|err| format!("connect handshake socket failed: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|err| format!("set read timeout failed: {err}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|err| format!("set write timeout failed: {err}"))?;

    let hello = HandshakeHello::new(agent_name.to_string());
    let line = to_json_line(&hello).map_err(|err| format!("encode hello failed: {err}"))?;
    stream
        .write_all(line.as_bytes())
        .map_err(|err| format!("send hello failed: {err}"))?;

    let mut reader = BufReader::new(stream);
    let mut ack_line = String::new();
    reader
        .read_line(&mut ack_line)
        .map_err(|err| format!("read ack failed: {err}"))?;
    let ack: HandshakeAck =
        from_json_line(&ack_line).map_err(|err| format!("decode ack failed: {err}"))?;

    if ack.magic != PROTOCOL_MAGIC || ack.status != "ok" {
        return Err(format!("invalid handshake ack payload: {:?}", ack));
    }
    Ok(ack)
}

fn run_phase2_input_channel(host_ipv4: &str, ack: &HandshakeAck, logger: &Logger) -> Result<(), String> {
    let config = EdgeSwitchConfig::right_default(1920);
    let cursor_x = config.screen_width_px - 1;
    let cursor_y = 540;
    if !should_switch_to_remote(cursor_x, &config) {
        logger.log("Edge switch not triggered; remote input channel idle.".to_string());
        return Ok(());
    }

    let frame = ControlFrame::new(
        ack.session_id.clone(),
        vec![
            ControlEvent::EdgeSwitch {
                edge: EdgeDirection::Right,
                cursor_x,
                cursor_y,
            },
            ControlEvent::MouseMove { dx: 14, dy: -3 },
            ControlEvent::MouseButton {
                button: "left".to_string(),
                pressed: true,
            },
            ControlEvent::MouseButton {
                button: "left".to_string(),
                pressed: false,
            },
        ],
    );

    let endpoint = format!("{}:{}", host_ipv4, ack.control_port);
    let mut stream = TcpStream::connect(endpoint.as_str())
        .map_err(|err| format!("connect control socket failed: {err}"))?;
    let encoded =
        to_json_line(&frame).map_err(|err| format!("encode control frame failed: {err}"))?;
    stream
        .write_all(encoded.as_bytes())
        .map_err(|err| format!("send control frame failed: {err}"))?;
    logger.log(format!(
        "Phase 2: edge-switch triggered, sent {} input events to control channel {}",
        frame.events.len(),
        endpoint
    ));
    Ok(())
}

fn run_phase2_with_retry(
    host_ipv4: &str,
    ack: &HandshakeAck,
    logger: &Logger,
    stop: &StopFlag,
) -> Result<(), String> {
    const BACKOFF_MS: [u64; 2] = [500, 1_000];

    for (index, delay_ms) in BACKOFF_MS.into_iter().enumerate() {
        if is_stopped(stop) {
            return Err("cancelado por el usuario".to_string());
        }
        match run_phase2_input_channel(host_ipv4, ack, logger) {
            Ok(()) => return Ok(()),
            Err(err) if is_transient_message(&err) => {
                log_session_event(&SessionEvent::ConnectionDropped {
                    peer_ip: host_ipv4.to_string(),
                    reason: err.clone(),
                    timestamp_ms: unix_timestamp_ms(),
                });
                logger.log(format!(
                    "[RECONNECT] attempt {}/{}, waiting {}ms...",
                    index + 1,
                    BACKOFF_MS.len(),
                    delay_ms
                ));
                thread::sleep(Duration::from_millis(delay_ms));
            }
            Err(err) => return Err(err),
        }
    }

    if is_stopped(stop) {
        return Err("cancelado por el usuario".to_string());
    }
    match run_phase2_input_channel(host_ipv4, ack, logger) {
        Ok(()) => Ok(()),
        Err(err) => {
            log_session_event(&SessionEvent::ConnectionDropped {
                peer_ip: host_ipv4.to_string(),
                reason: err.clone(),
                timestamp_ms: unix_timestamp_ms(),
            });
            logger.log(format!(
                "[RECONNECT] failed after {} attempts, session ended",
                BACKOFF_MS.len() + 1
            ));
            Err(err)
        }
    }
}

fn run_phase3_stream_channel(
    host_ipv4: &str,
    agent_name: &str,
    ack: &HandshakeAck,
    logger: &Logger,
    stop: &StopFlag,
    config: &AgentConfig,
) -> Result<(), String> {
    const MAX_CONSECUTIVE_STREAM_TIMEOUTS: u32 = 4;
    let endpoint = format!("{}:{}", host_ipv4, ack.stream_port);
    let mut stream = TcpStream::connect(endpoint.as_str())
        .map_err(|err| format!("connect stream socket failed: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|err| format!("set stream read timeout failed: {err}"))?;

    let hello = StreamHello::new(ack.session_id.clone(), agent_name.to_string());
    let hello_line =
        to_json_line(&hello).map_err(|err| format!("encode stream hello failed: {err}"))?;
    stream
        .write_all(hello_line.as_bytes())
        .map_err(|err| format!("send stream hello failed: {err}"))?;

    let mut reader = BufReader::new(stream);
    let render_enabled = config.render_enabled;
    let mut renderer: Option<FrameRenderer> = None;
    let mut metrics = StreamMetrics::new();
    let target_frames = config.target_stream_frames;

    // Open persistent control connection once for the session lifetime.
    let control_endpoint = format!("{}:{}", host_ipv4, ack.control_port);
    let mut control_stream = TcpStream::connect(control_endpoint.as_str())
        .map_err(|err| format!("connect persistent control socket failed: {err}"))?;
    control_stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("set control write timeout failed: {err}"))?;

    let mut received = 0_u32;
    let mut black_filtered = 0_u32;
    let mut timeout_streak = 0_u32;
    let mut last_sequence = 0_u64;
    let mut total_raw = 0_usize;
    let mut last_feedback: Option<(u32, u8)> = None;
    let mut black_filter_logged = false;
    let mut last_compat_mode: Option<bool> = None;

    while target_frames == 0 || received < target_frames {
        if is_stopped(stop) {
            logger.log("Transmisión cancelada por el usuario.".to_string());
            break;
        }
        let mut line = String::new();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                timeout_streak += 1;
                if timeout_streak >= MAX_CONSECUTIVE_STREAM_TIMEOUTS {
                    return Err("stream timeout waiting for frames".to_string());
                }
                continue;
            }
            Err(err) => return Err(format!("read stream frame failed: {err}")),
        };
        if bytes_read == 0 {
            break;
        }
        timeout_streak = 0;
        let frame: StreamFrame =
            from_json_line(&line).map_err(|err| format!("decode stream frame failed: {err}"))?;
        if frame.magic != PROTOCOL_MAGIC || frame.session_id != ack.session_id {
            return Err(format!("invalid stream frame payload: {:?}", frame));
        }
        let compat_mode = frame.source.eq_ignore_ascii_case("synthetic");
        if last_compat_mode != Some(compat_mode) {
            logger.log(if compat_mode {
                "Modo compatibilidad activo (sin captura real).".to_string()
            } else {
                "Captura real del equipo remoto activa.".to_string()
            });
            last_compat_mode = Some(compat_mode);
        }

        let raw = decode_stream_frame(&frame)?;
        if is_mostly_black_frame(&raw, frame.stride_bytes, frame.width as usize, frame.height as usize) {
            black_filtered += 1;
            if !black_filter_logged {
                logger.log("Pantalla negra detectada: ocultando frames sin imagen.".to_string());
                black_filter_logged = true;
            }
            continue;
        }
        if render_enabled {
            if renderer.is_none() {
                renderer = Some(FrameRenderer::try_new()?);
            }
            if let Some(renderer) = renderer.as_mut() {
                renderer.render(&frame, &raw, compat_mode)?;
                if !renderer.is_open() {
                    break;
                }
            }
        }

        received += 1;
        last_sequence = frame.sequence;
        total_raw += raw.len();
        metrics.observe(frame.captured_at_ms, frame.frame_interval_ms);

        if received % 10 == 0 {
            let summary = metrics.summary();
            let (target_fps, scale_divisor) = choose_adaptive_target(&summary);
            let next_feedback = (target_fps, scale_divisor);
            if last_feedback != Some(next_feedback) {
                send_stream_feedback(&mut control_stream, ack, &summary, target_fps, scale_divisor)?;
                last_feedback = Some(next_feedback);
            }
        }
    }

    let summary = metrics.summary();
    logger.log(format!(
        "Phase 5: stream/render active, frames={} black_filtered={} last_seq={} raw={} fps={:.2} avg_latency_ms={:.2} jitter_ms={:.2}",
        received, black_filtered, last_sequence, total_raw, summary.fps, summary.avg_latency_ms, summary.jitter_ms
    ));
    if received == 0 {
        if black_filtered > 0 {
            return Err(
                "Conectado, pero el equipo remoto solo está enviando pantallas negras. Revisa permisos de captura de pantalla en el host (RDP/minimizado/bloqueo)."
                    .to_string(),
            );
        }
        return Err(
            "No se recibieron frames del equipo remoto. Revisa que el host tenga permisos de captura de pantalla y que no haya bloqueo de red/firewall."
                .to_string(),
        );
    }
    Ok(())
}

fn run_phase3_with_retry(
    host_ipv4: &str,
    agent_name: &str,
    ack: &HandshakeAck,
    logger: &Logger,
    stop: &StopFlag,
    config: &AgentConfig,
) -> Result<(), String> {
    const BACKOFF_MS: [u64; 3] = [500, 1_000, 2_000];

    for (index, delay_ms) in BACKOFF_MS.into_iter().enumerate() {
        if is_stopped(stop) {
            return Err("cancelado por el usuario".to_string());
        }
        match run_phase3_stream_channel(host_ipv4, agent_name, ack, logger, stop, config) {
            Ok(()) => return Ok(()),
            Err(err) if is_transient_message(&err) => {
                log_session_event(&SessionEvent::ConnectionDropped {
                    peer_ip: host_ipv4.to_string(),
                    reason: err.clone(),
                    timestamp_ms: unix_timestamp_ms(),
                });
                logger.log(format!(
                    "[RECONNECT] attempt {}/{}, waiting {}ms...",
                    index + 1,
                    BACKOFF_MS.len(),
                    delay_ms
                ));
                thread::sleep(Duration::from_millis(delay_ms));
            }
            Err(err) => return Err(err),
        }
    }

    if is_stopped(stop) {
        return Err("cancelado por el usuario".to_string());
    }
    match run_phase3_stream_channel(host_ipv4, agent_name, ack, logger, stop, config) {
        Ok(()) => Ok(()),
        Err(err) => {
            log_session_event(&SessionEvent::ConnectionDropped {
                peer_ip: host_ipv4.to_string(),
                reason: err.clone(),
                timestamp_ms: unix_timestamp_ms(),
            });
            logger.log(format!(
                "[RECONNECT] failed after {} attempts, session ended",
                BACKOFF_MS.len() + 1
            ));
            Err(err)
        }
    }
}

fn is_transient_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}

fn is_transient_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if lower.contains("no se recibieron frames")
        || lower.contains("pantallas negras")
        || lower.contains("stream timeout waiting for frames")
    {
        return true;
    }
    let kind = if lower.contains("timed out") || lower.contains("timeout") {
        std::io::ErrorKind::TimedOut
    } else if lower.contains("would block") {
        std::io::ErrorKind::WouldBlock
    } else if lower.contains("connection reset") {
        std::io::ErrorKind::ConnectionReset
    } else if lower.contains("connection aborted") {
        std::io::ErrorKind::ConnectionAborted
    } else if lower.contains("broken pipe") {
        std::io::ErrorKind::BrokenPipe
    } else {
        return false;
    };
    is_transient_error(&std::io::Error::from(kind))
}

fn choose_adaptive_target(summary: &StreamSummary) -> (u32, u8) {
    if summary.avg_latency_ms > 240.0 || summary.jitter_ms > 150.0 {
        (5, 3)
    } else if summary.avg_latency_ms > 170.0 || summary.jitter_ms > 100.0 {
        (7, 2)
    } else if summary.avg_latency_ms > 120.0 || summary.jitter_ms > 70.0 {
        (9, 2)
    } else {
        (12, 1)
    }
}

fn send_stream_feedback(
    control: &mut TcpStream,
    ack: &HandshakeAck,
    summary: &StreamSummary,
    target_fps: u32,
    scale_divisor: u8,
) -> Result<(), String> {
    let feedback = ControlFrame::new(
        ack.session_id.clone(),
        vec![ControlEvent::StreamFeedback {
            target_fps,
            scale_divisor,
            avg_latency_ms: summary.avg_latency_ms.round() as u32,
            jitter_ms: summary.jitter_ms.round() as u32,
        }],
    );
    let encoded =
        to_json_line(&feedback).map_err(|err| format!("encode feedback frame failed: {err}"))?;
    control
        .write_all(encoded.as_bytes())
        .map_err(|err| format!("send feedback frame failed: {err}"))?;
    Ok(())
}

fn decode_stream_frame(frame: &StreamFrame) -> Result<Vec<u8>, String> {
    match frame.compression {
        StreamCompression::None => {
            if frame.compressed_payload_b64.is_empty() {
                if frame.source.eq_ignore_ascii_case("synthetic") {
                    // Synthetic frame — generate animated test pattern.
                    Ok(generate_synthetic_bgra(frame))
                } else {
                    Err(format!(
                        "empty uncompressed frame payload for source '{}'",
                        frame.source
                    ))
                }
            } else {
                // Uncompressed real frame — just base64-decode the payload.
                BASE64
                    .decode(frame.compressed_payload_b64.as_bytes())
                    .map_err(|err| format!("base64 decode (uncompressed) failed: {err}"))
            }
        }
        StreamCompression::Lz4 => {
            let compressed = BASE64
                .decode(frame.compressed_payload_b64.as_bytes())
                .map_err(|err| format!("base64 decode failed: {err}"))?;
            let raw = decompress_size_prepended(&compressed)
                .map_err(|err| format!("lz4 decompress failed: {err}"))?;
            if frame.raw_len != 0 && raw.len() != frame.raw_len {
                return Err(format!(
                    "raw length mismatch expected={} got={}",
                    frame.raw_len,
                    raw.len()
                ));
            }
            Ok(raw)
        }
    }
}

fn generate_synthetic_bgra(frame: &StreamFrame) -> Vec<u8> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let mut raw = vec![0_u8; width * height * 4];
    let pulse = ((frame.sequence / 6) % 32) as u8;
    let base = 26u8.saturating_add(pulse);
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let checker = if ((x / 28) + (y / 28)) % 2 == 0 { 10 } else { 0 };
            let value = base.saturating_add(checker);
            // Neutral grayscale compatibility frame: avoids flashy fallback colors.
            raw[idx] = value;
            raw[idx + 1] = value;
            raw[idx + 2] = value;
            raw[idx + 3] = 255;
        }
    }
    raw
}

struct FrameRenderer {
    window: Window,
    width: usize,
    height: usize,
    buffer: Vec<u32>,
    last_compat_mode: Option<bool>,
}

impl FrameRenderer {
    fn try_new() -> Result<Self, String> {
        let width = 1280;
        let height = 720;
        let options = WindowOptions {
            resize: true,
            scale: Scale::X1,
            ..WindowOptions::default()
        };
        let mut window = Window::new("LanPilot Agent Stream", width, height, options)
            .map_err(|err| format!("create render window failed: {err}"))?;
        window.set_target_fps(60);
        Ok(Self {
            window,
            width,
            height,
            buffer: vec![0_u32; width * height],
            last_compat_mode: None,
        })
    }

    fn render(&mut self, frame: &StreamFrame, raw: &[u8], compat_mode: bool) -> Result<(), String> {
        let width = frame.width as usize;
        let height = frame.height as usize;
        let stride_bytes = frame.stride_bytes;
        if stride_bytes < width * 4 {
            return Err(format!(
                "invalid stride {} (expected at least {})",
                stride_bytes,
                width * 4
            ));
        }
        if raw.len() < stride_bytes * height {
            return Err(format!(
                "insufficient raw data {} for stride {} and height {}",
                raw.len(),
                stride_bytes,
                height
            ));
        }

        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.buffer.resize(width * height, 0);
        }
        if self.last_compat_mode != Some(compat_mode) {
            self.window.set_title(if compat_mode {
                "LanPilot Agent Stream — Modo compatibilidad"
            } else {
                "LanPilot Agent Stream — Captura real"
            });
            self.last_compat_mode = Some(compat_mode);
        }

        for y in 0..height {
            let row_start = y * stride_bytes;
            for x in 0..width {
                let idx = row_start + x * 4;
                let b = raw[idx] as u32;
                let g = raw[idx + 1] as u32;
                let r = raw[idx + 2] as u32;
                self.buffer[y * width + x] = (r << 16) | (g << 8) | b;
            }
        }

        self.window
            .update_with_buffer(&self.buffer, width, height)
            .map_err(|err| format!("render update failed: {err}"))
    }

    fn is_open(&self) -> bool {
        self.window.is_open()
    }
}

fn is_mostly_black_frame(raw: &[u8], stride_bytes: usize, width: usize, height: usize) -> bool {
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

struct StreamMetrics {
    started_at: Instant,
    previous_arrival: Option<Instant>,
    frame_count: u64,
    total_latency_ms: f64,
    jitter_ms: f64,
}

impl StreamMetrics {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            previous_arrival: None,
            frame_count: 0,
            total_latency_ms: 0.0,
            jitter_ms: 0.0,
        }
    }

    fn observe(&mut self, captured_at_ms: u128, frame_interval_ms: u32) {
        let now = Instant::now();
        self.frame_count += 1;

        let now_ms = unix_timestamp_ms() as f64;
        let captured_ms = captured_at_ms as f64;
        if now_ms >= captured_ms {
            self.total_latency_ms += now_ms - captured_ms;
        }

        if let Some(previous) = self.previous_arrival {
            let interval_ms = (now - previous).as_secs_f64() * 1000.0;
            let expected_ms = frame_interval_ms as f64;
            let deviation = (interval_ms - expected_ms).abs();
            self.jitter_ms = (self.jitter_ms * 0.85) + (deviation * 0.15);
        }
        self.previous_arrival = Some(now);
    }

    fn summary(&self) -> StreamSummary {
        let elapsed = self.started_at.elapsed().as_secs_f64().max(0.001);
        let fps = self.frame_count as f64 / elapsed;
        let avg_latency_ms = if self.frame_count == 0 {
            0.0
        } else {
            self.total_latency_ms / self.frame_count as f64
        };
        StreamSummary {
            fps,
            avg_latency_ms,
            jitter_ms: self.jitter_ms,
        }
    }
}

struct StreamSummary {
    fps: f64,
    avg_latency_ms: f64,
    jitter_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_error_kinds_are_detected() {
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
        ] {
            assert!(is_transient_error(&std::io::Error::from(kind)));
        }
    }

    #[test]
    fn non_transient_errors_are_rejected() {
        assert!(!is_transient_error(&std::io::Error::from(
            std::io::ErrorKind::InvalidData,
        )));
    }

    #[test]
    fn no_frame_messages_are_treated_as_transient_for_retry() {
        assert!(is_transient_message("No se recibieron frames del equipo remoto."));
        assert!(is_transient_message(
            "Conectado, pero el equipo remoto solo está enviando pantallas negras."
        ));
        assert!(is_transient_message("stream timeout waiting for frames"));
    }

    #[test]
    fn order_host_candidates_prefers_exact_ip_then_same_name() {
        let responses = vec![
            DiscoveryResponse::new("PC-1", "192.168.1.30"),
            DiscoveryResponse::new("PC-OBJETIVO", "192.168.1.40"),
            DiscoveryResponse::new("PC-OBJETIVO", "192.168.1.50"),
        ];
        let ordered = order_host_candidates(responses, Some("192.168.1.50"), Some("PC-OBJETIVO"));
        assert_eq!(ordered[0].host_ipv4, "192.168.1.50");
        assert_eq!(ordered[1].host_ipv4, "192.168.1.40");
    }

    #[test]
    fn order_host_candidates_returns_empty_when_only_ip_missing() {
        let responses = vec![
            DiscoveryResponse::new("PC-1", "192.168.1.30"),
            DiscoveryResponse::new("PC-2", "192.168.1.40"),
        ];
        let ordered = order_host_candidates(responses, Some("192.168.1.99"), None);
        assert!(ordered.is_empty());
    }

    #[test]
    fn apply_probe_priority_moves_reachable_hosts_first_by_latency() {
        let candidates = vec![
            DiscoveryResponse::new("PC-A", "192.168.1.10"),
            DiscoveryResponse::new("PC-B", "192.168.1.11"),
            DiscoveryResponse::new("PC-C", "192.168.1.12"),
        ];
        let probe_results = vec![
            HandshakeProbeResult {
                index: 2,
                reachable: true,
                latency_ms: 15,
            },
            HandshakeProbeResult {
                index: 0,
                reachable: true,
                latency_ms: 40,
            },
            HandshakeProbeResult {
                index: 1,
                reachable: false,
                latency_ms: 20,
            },
        ];

        let ordered = apply_probe_priority(candidates, &probe_results);
        assert_eq!(ordered[0].host_ipv4, "192.168.1.12");
        assert_eq!(ordered[1].host_ipv4, "192.168.1.10");
        assert_eq!(ordered[2].host_ipv4, "192.168.1.11");
    }

    #[test]
    fn handshake_probe_timeout_shrinks_with_many_candidates() {
        assert_eq!(handshake_probe_timeout(2), Duration::from_millis(450));
        assert_eq!(handshake_probe_timeout(6), Duration::from_millis(320));
        assert_eq!(handshake_probe_timeout(12), Duration::from_millis(240));
    }

    #[test]
    fn agent_config_with_pair_code_has_sane_defaults() {
        let config = AgentConfig::with_pair_code("123456");
        assert_eq!(config.pair_code, "123456");
        assert!(config.agent_name.is_none());
        assert!(config.preferred_host_ipv4.is_none());
        assert!(config.preferred_host_name.is_none());
        assert!(config.render_enabled);
        assert_eq!(config.target_stream_frames, 60);
    }

    #[test]
    fn run_agent_rejects_invalid_pair_code() {
        let logger = Logger::new(|_| {});
        let stop = lanpilot_core::new_stop_flag();
        let config = AgentConfig::with_pair_code("abc");
        let result = run_agent(config, logger, stop);
        assert!(result.is_err(), "non 6-digit pair codes must be rejected before any network I/O");
    }

    #[test]
    fn run_agent_honors_pre_set_stop_flag_before_any_network_io() {
        let logger = Logger::new(|_| {});
        let stop = lanpilot_core::new_stop_flag();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let config = AgentConfig::with_pair_code("123456");

        let started = Instant::now();
        let result = run_agent(config, logger, stop);
        assert!(result.is_err(), "pre-cancelled run_agent must return an error, not attempt to connect");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation before discovery must be immediate"
        );
    }

    #[test]
    fn decode_stream_frame_rejects_empty_payload_for_non_synthetic_source() {
        let mut frame = StreamFrame::synthetic("lp-xyz", 1);
        frame.source = "screen".to_string();
        let result = decode_stream_frame(&frame);
        assert!(
            result.is_err(),
            "empty payload must only be accepted for synthetic frames"
        );
    }

    #[test]
    fn decode_stream_frame_accepts_empty_payload_for_synthetic_source() {
        let frame = StreamFrame::synthetic("lp-xyz", 1);
        let result = decode_stream_frame(&frame);
        assert!(result.is_ok(), "synthetic frame should decode to generated pixels");
    }

    #[test]
    fn black_frame_detector_flags_black_payload() {
        let width = 64usize;
        let height = 32usize;
        let stride = width * 4;
        let raw = vec![0_u8; stride * height];
        assert!(is_mostly_black_frame(&raw, stride, width, height));
    }

    #[test]
    fn black_frame_detector_ignores_bright_payload() {
        let width = 64usize;
        let height = 32usize;
        let stride = width * 4;
        let mut raw = vec![0_u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let idx = y * stride + x * 4;
                raw[idx] = 255;
                raw[idx + 1] = 255;
                raw[idx + 2] = 255;
                raw[idx + 3] = 255;
            }
        }
        assert!(!is_mostly_black_frame(&raw, stride, width, height));
    }
}
