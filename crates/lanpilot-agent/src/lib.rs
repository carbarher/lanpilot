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
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lanpilot_core::{
    ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse, EdgeDirection,
    EdgeSwitchConfig, HandshakeAck, HandshakeHello, Logger, PRODUCT_NAME, PROTOCOL_MAGIC, StopFlag,
    SessionEvent, StreamCompression, StreamFrame, StreamHello, TAGLINE, from_json_line, is_stopped,
    log_session_event, normalize_pair_code, should_switch_to_remote, to_json_line, unix_timestamp_ms,
    AUDIO_PORT,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct RecentHost {
    ip: String,
    host_name: String,
    last_connected: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct RecentHostsList {
    hosts: Vec<RecentHost>,
}

fn load_recent_hosts() -> RecentHostsList {
    let path = "C:\\Users\\carlo\\.gemini\\antigravity\\recent_hosts.json";
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(list) = serde_json::from_str::<RecentHostsList>(&content) {
            return list;
        }
    }
    RecentHostsList::default()
}

fn save_recent_hosts(list: &RecentHostsList) {
    let path = "C:\\Users\\carlo\\.gemini\\antigravity\\recent_hosts.json";
    let _ = std::fs::create_dir_all("C:\\Users\\carlo\\.gemini\\antigravity");
    if let Ok(serialized) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(path, serialized);
    }
}

/// Run the LanPilot agent connection flow to completion (or until `stop` is
/// set / an unrecoverable error occurs).
///
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

/// This function blocks the calling thread; run it on a background thread
/// (as `lanpilot-app` does) if the caller also needs to keep servicing a UI.
pub fn run_agent(mut config: AgentConfig, logger: Logger, stop: StopFlag) -> Result<(), String> {
    #[cfg(windows)]
    make_process_dpi_aware();
    let agent_name = config.agent_name.clone().unwrap_or_else(|| "lanpilot-agent".to_string());
    let pair_code = normalize_pair_code(&config.pair_code)
        .ok_or_else(|| "el código de emparejamiento debe tener exactamente 6 dígitos".to_string())?;

    if config.preferred_host_ipv4.is_none() {
        let list = load_recent_hosts();
        if !list.hosts.is_empty() {
            println!("\n=== LanPilot Agent — Conexión Rápida ===");
            println!("Selecciona una conexión reciente del historial o introduce una nueva IP:");
            for (i, host) in list.hosts.iter().enumerate() {
                println!("[{}] {} ({})", i + 1, host.ip, host.host_name);
            }
            println!("[0] Introducir nueva dirección IP manual");
            print!("> ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            
            let mut choice = String::new();
            if std::io::stdin().read_line(&mut choice).is_ok() {
                let trimmed = choice.trim();
                if let Ok(num) = trimmed.parse::<usize>() {
                    if num > 0 && num <= list.hosts.len() {
                        let selected_ip = list.hosts[num - 1].ip.clone();
                        let selected_name = list.hosts[num - 1].host_name.clone();
                        logger.log(format!("Seleccionado del historial: {} ({})", selected_ip, selected_name));
                        config.preferred_host_ipv4 = Some(selected_ip);
                        config.preferred_host_name = Some(selected_name);
                    }
                } else if !trimmed.is_empty() && trimmed != "0" {
                    config.preferred_host_ipv4 = Some(trimmed.to_string());
                }
            }
        }
    }

    if let Some(ref ip_str) = config.preferred_host_ipv4 {
        if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
            if !lanpilot_core::is_private_ip(ip) {
                return Err(format!("Dirección IP destino no es una IP privada local válida: {}", ip_str));
            }
        }
    }

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
        if let Ok(ip) = discovered.host_ipv4.parse::<std::net::IpAddr>() {
            if !lanpilot_core::is_private_ip(ip) {
                logger.log(format!("Ignorando candidato de IP pública no autorizada: {}", discovered.host_ipv4));
                continue;
            }
        } else {
            continue;
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

        // Registrar host en el historial recent_hosts.json
        {
            let connected_ip = discovered.host_ipv4.clone();
            let mut list = load_recent_hosts();
            list.hosts.retain(|h| h.ip != connected_ip);
            list.hosts.insert(0, RecentHost {
                ip: connected_ip,
                host_name: ack.host_name.clone(),
                last_connected: unix_timestamp_ms() as u64,
            });
            list.hosts.truncate(5);
            save_recent_hosts(&list);
        }

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
    let mut ordered = order_host_candidates(responses, preferred_host_ipv4, preferred_host_name);
    if ordered.is_empty() {
        if let Some(preferred_ip) = preferred_host_ipv4 {
            logger.log(format!(
                "Discovery UDP no encontró hosts. Creando candidato virtual para conexión directa: {preferred_ip}"
            ));
            ordered.push(DiscoveryResponse {
                magic: PROTOCOL_MAGIC.to_string(),
                host_name: preferred_host_name.unwrap_or("preferred-host").to_string(),
                host_ipv4: preferred_ip.to_string(),
                handshake_port: lanpilot_core::HANDSHAKE_PORT,
            });
        } else {
            if let Some(preferred_name) = preferred_host_name {
                return Err(format!(
                    "No se encontró el equipo seleccionado ({preferred_name}) en la red local."
                ));
            }
            return Err("No se encontró ningún host LanPilot en la red local.".to_string());
        }
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
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .map_err(|err| format!("set read timeout failed: {err}"))?;

    let probe = DiscoveryProbe::new(agent_name.to_string(), pair_code.to_string());
    let payload = to_json_line(&probe).map_err(|err| format!("encode probe failed: {err}"))?;
    
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT))];
    if let Some(local_ip) = lanpilot_core::local_ipv4() {
        let octets = local_ip.octets();
        let subnet_broadcast = Ipv4Addr::new(octets[0], octets[1], octets[2], 255);
        targets.push(SocketAddr::from((subnet_broadcast, DISCOVERY_PORT)));
    }

    let socket_clone = socket.try_clone().map_err(|err| format!("clone discovery socket failed: {err}"))?;
    let by_ip = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let by_ip_clone = std::sync::Arc::clone(&by_ip);

    let listener_handle = std::thread::spawn(move || {
        let mut buf = [0_u8; 2048];
        while let Ok((received, source)) = socket_clone.recv_from(&mut buf) {
            if let Ok(response_line) = std::str::from_utf8(&buf[..received]) {
                if let Ok(response) = from_json_line::<DiscoveryResponse>(response_line) {
                    if response.magic == PROTOCOL_MAGIC {
                        if let Ok(claimed_ip) = response.host_ipv4.parse::<std::net::IpAddr>() {
                            if source.ip() == claimed_ip {
                                if let Ok(mut guard) = by_ip_clone.lock() {
                                    guard.insert(response.host_ipv4.clone(), response);
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let started = Instant::now();
    let mut last_send = Instant::now() - Duration::from_secs(1);

    while started.elapsed() < timeout {
        if last_send.elapsed() >= Duration::from_millis(400) {
            for target in &targets {
                let _ = socket.send_to(payload.as_bytes(), *target);
            }
            last_send = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(socket);
    let _ = listener_handle.join();

    let final_map = std::sync::Arc::try_unwrap(by_ip)
        .map_err(|_| "failed to unwrap responses mutex".to_string())?
        .into_inner()
        .map_err(|_| "failed to get inner responses map".to_string())?;

    let mut responses: Vec<DiscoveryResponse> = final_map.into_values().collect();
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
    let _ = stream.set_nodelay(true);
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

struct RenderJob {
    buffer: Vec<u32>,
    width: usize,
    height: usize,
    title: String,
    compat_mode: bool,
}

#[allow(unused_assignments)]
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
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|err| format!("set stream read timeout failed: {err}"))?;

    let mut stream_cipher_rx = lanpilot_core::Rc4Cipher::new(format!("{}-stream-h2c", config.pair_code).as_bytes());
    let mut stream_cipher_tx = lanpilot_core::Rc4Cipher::new(format!("{}-stream-c2h", config.pair_code).as_bytes());

    let hello = StreamHello::new(ack.session_id.clone(), agent_name.to_string());
    let hello_line =
        to_json_line(&hello).map_err(|err| format!("encode stream hello failed: {err}"))?;
    
    let encrypted_hello = lanpilot_core::encrypt_line(&hello_line, &mut stream_cipher_tx);
    stream
        .write_all(encrypted_hello.as_bytes())
        .map_err(|err| format!("send stream hello failed: {err}"))?;

    let mut reader = BufReader::new(stream);
    let render_enabled = config.render_enabled;
    let mut metrics = StreamMetrics::new();
    let target_frames = config.target_stream_frames;

    let control_endpoint = format!("{}:{}", host_ipv4, ack.control_port);
    let mut control_stream = TcpStream::connect(control_endpoint.as_str())
        .map_err(|err| format!("connect persistent control socket failed: {err}"))?;
    let _ = control_stream.set_nodelay(true);
    control_stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("set control write timeout failed: {err}"))?;

    let control_cipher_tx = lanpilot_core::Rc4Cipher::new(format!("{}-control-c2h", config.pair_code).as_bytes());
    let control_tx_mutex = Arc::new(std::sync::Mutex::new((control_stream.try_clone().unwrap(), control_cipher_tx)));

    spawn_audio_client(host_ipv4.to_string(), stop.clone(), logger.clone(), config.pair_code.clone());

    let mut received = 0_u32;
    #[allow(unused_variables)]
    let mut black_filtered = 0_u32;
    let mut timeout_streak = 0_u32;
    #[allow(unused_variables)]
    let mut last_sequence = 0_u64;
    #[allow(unused_variables)]
    let mut total_raw = 0_usize;
    let mut last_feedback: Option<(u32, u8)> = None;
    let mut black_filter_logged = false;
    let mut _last_compat_mode: Option<bool> = None;
    #[allow(unused_assignments)]
    let mut last_captured_at_ms = 0_u64;
    let mut use_rgb565 = false;
    let mut last_clipboard_image_hash: Option<u64> = None;
    let mut last_html_hash: Option<u64> = None;
    let mut last_rtf_hash: Option<u64> = None;
    let mut last_clipboard_image_check = std::time::Instant::now();
    let mut last_quality_adjustment = std::time::Instant::now();
    let mut privacy_mode_active = false;
    let mut last_frame_received_at = std::time::Instant::now();
    let mut local_clipboard = arboard::Clipboard::new().ok();
    let mut last_sent_clipboard = String::new();

    let (render_tx, render_rx) = std::sync::mpsc::sync_channel::<RenderJob>(2);
    let (buffer_pool_tx, buffer_pool_rx) = std::sync::mpsc::channel::<Vec<u32>>();
    let (input_tx, input_rx) = std::sync::mpsc::channel::<ControlEvent>();

    let _ = buffer_pool_tx.send(vec![0; 1920 * 1080]);
    let _ = buffer_pool_tx.send(vec![0; 1920 * 1080]);

    let stop_render = stop.clone();
    let logger_render = logger.clone();
    let input_tx_render = input_tx.clone();
    let session_id_render = ack.session_id.clone();
    let control_tx_mutex_render = Arc::clone(&control_tx_mutex);
    
    let _render_thread = std::thread::spawn(move || {
        let input_tx = input_tx_render;
        let mut renderer: Option<FrameRenderer> = None;
        let mut last_mouse_pos: Option<(f32, f32)> = None;
        let mut edge_touch_start: Option<std::time::Instant> = None;
        let mut last_left_down = false;
        let mut last_right_down = false;
        let mut last_middle_down = false;
        let mut last_keys = std::collections::HashSet::<minifb::Key>::new();
        let mut relative_mouse_mode = false;
        let mut last_lock_state_check = std::time::Instant::now();
        let mut last_caps_lock = None;
        let mut last_num_lock = None;

        let char_buffer_local = std::sync::Arc::new(std::sync::Mutex::new(Vec::<char>::new()));
        let char_buffer_window = std::sync::Arc::clone(&char_buffer_local);

        let mut frame_queue = std::collections::VecDeque::new();
        let mut last_scheduled_draw = std::time::Instant::now();

        while !is_stopped(&stop_render) {
            let tick_start = std::time::Instant::now();

            while let Ok(job) = render_rx.try_recv() {
                let interval = std::time::Duration::from_millis(16);
                let target_time = last_scheduled_draw.max(std::time::Instant::now()) + interval;
                last_scheduled_draw = target_time;
                frame_queue.push_back((target_time, job));
            }

            let now = std::time::Instant::now();
            let mut drew_frame = false;
            
            if let Some(&(target_time, _)) = frame_queue.front() {
                if now >= target_time {
                    if let Some((_, job)) = frame_queue.pop_front() {
                        if renderer.is_none() {
                            if let Ok(mut r) = FrameRenderer::try_new(job.width, job.height) {
                                r.char_buffer = std::sync::Arc::clone(&char_buffer_window);
                                renderer = Some(r);
                            }
                        }
                        if let Some(ref mut r) = renderer {
                            if r.width != job.width || r.height != job.height {
                                r.width = job.width;
                                r.height = job.height;
                                r.buffer.resize(job.width * job.height, 0);
                            }
                            r.buffer.copy_from_slice(&job.buffer);
                            r.last_compat_mode = Some(job.compat_mode);
                            r.window.set_title(&job.title);
                            let _ = r.window.update_with_buffer(&r.buffer, job.width, job.height);
                            let _ = buffer_pool_tx.send(job.buffer);
                            drew_frame = true;
                        }
                    }
                }
            }

            if !drew_frame {
                if let Some(ref mut r) = renderer {
                    let _ = r.window.update();
                }
            }

            if let Some(ref mut r) = renderer {
                if !r.window.is_open() {
                    break;
                }

                let ctrl = r.window.is_key_down(minifb::Key::LeftCtrl) || r.window.is_key_down(minifb::Key::RightCtrl);
                let alt = r.window.is_key_down(minifb::Key::LeftAlt) || r.window.is_key_down(minifb::Key::RightAlt);
                let enter = r.window.is_key_pressed(minifb::Key::Enter, minifb::KeyRepeat::No);

                if ctrl && alt && enter {
                    let _ = r.toggle_fullscreen();
                }
                if ctrl && alt && r.window.is_key_down(minifb::Key::Q) {
                    logger_render.log("Desconexión de pánico iniciada por el usuario (Ctrl + Alt + Q).".to_string());
                    break;
                }
                if ctrl && alt && r.window.is_key_pressed(minifb::Key::M, minifb::KeyRepeat::No) {
                    logger_render.log("Solicitando rotar de monitor capturado (Ctrl + Alt + M).".to_string());
                    let _ = input_tx.send(ControlEvent::CycleMonitor);
                }
                if ctrl && alt && r.window.is_key_pressed(minifb::Key::L, minifb::KeyRepeat::No) {
                    relative_mouse_mode = !relative_mouse_mode;
                    r.window.set_cursor_visibility(!relative_mouse_mode);
                    logger_render.log(format!("Modo ratón relativo alternado (Pointer Lock): {}", relative_mouse_mode));
                }
                if ctrl && alt && r.window.is_key_pressed(minifb::Key::P, minifb::KeyRepeat::No) {
                    let _ = input_tx.send(ControlEvent::TogglePrivacyMode { enabled: true });
                }
                if ctrl && alt && r.window.is_key_pressed(minifb::Key::K, minifb::KeyRepeat::No) {
                    let _ = input_tx.send(ControlEvent::SetVideoFormat { use_rgb565: true });
                }
                if ctrl && alt && r.window.is_key_pressed(minifb::Key::F, minifb::KeyRepeat::No) {
                    let _ = input_tx.send(ControlEvent::FileChunk { filename: "trigger_select_file".to_string(), offset: 0, total_size: 0, data_b64: String::new() });
                }

                if ctrl && !alt && r.window.is_key_pressed(minifb::Key::V, minifb::KeyRepeat::No) {
                    if let Some(files) = lanpilot_core::read_clipboard_files() {
                        if !files.is_empty() {
                            logger_render.log(format!("Zero-Click File Paste: detectados {} archivos locales para pegar.", files.len()));
                            let tx_mutex_clone = Arc::clone(&control_tx_mutex_render);
                            let session_id = session_id_render.clone();
                            let logger_thread = logger_render.clone();
                            std::thread::spawn(move || {
                                for path_str in files {
                                    let path = std::path::Path::new(&path_str);
                                    if !path.exists() { continue; }
                                    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("archivo").to_string();
                                    let total_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                                    logger_thread.log(format!("Transfiriendo archivo local '{}' ({:.1} MB)...", filename, total_size as f64 / 1024.0 / 1024.0));
                                    
                                    if let Ok(mut file) = std::fs::File::open(&path) {
                                        use std::io::Read;
                                        use std::io::Write;
                                        let mut buffer = vec![0u8; 32768];
                                        let mut offset = 0_u64;
                                        while let Ok(n) = file.read(&mut buffer) {
                                            if n == 0 { break; }
                                            let chunk_b64 = BASE64.encode(&buffer[0..n]);
                                            let chunk_event = ControlEvent::FileChunk {
                                                filename: filename.clone(),
                                                offset,
                                                total_size,
                                                data_b64: chunk_b64,
                                            };
                                            let control_frame = ControlFrame::new(session_id.clone(), vec![chunk_event]);
                                            if let Ok(line) = to_json_line(&control_frame) {
                                                let mut guard = tx_mutex_clone.lock().unwrap();
                                                let encrypted = lanpilot_core::encrypt_line(&line, &mut guard.1);
                                                let _ = guard.0.write_all(encrypted.as_bytes());
                                            }
                                            offset += n as u64;
                                            std::thread::sleep(std::time::Duration::from_millis(5));
                                        }
                                        
                                        // Notificar fin de transferencia para inyección de clipboard en Host
                                        let finished_event = ControlEvent::FileTransferFinished {
                                            filename: filename.clone(),
                                            temp_path: String::new(),
                                        };
                                        let finished_frame = ControlFrame::new(session_id.clone(), vec![finished_event]);
                                        if let Ok(line) = to_json_line(&finished_frame) {
                                            let mut guard = tx_mutex_clone.lock().unwrap();
                                            let encrypted = lanpilot_core::encrypt_line(&line, &mut guard.1);
                                            let _ = guard.0.write_all(encrypted.as_bytes());
                                        }
                                        logger_thread.log(format!("Archivo '{}' transferido exitosamente.", filename));
                                    }
                                }
                            });
                        }
                    }
                }

                #[cfg(windows)]
                {
                    if last_lock_state_check.elapsed() >= std::time::Duration::from_millis(500) {
                        last_lock_state_check = std::time::Instant::now();
                        unsafe {
                            let caps = (GetKeyState(0x14) & 1) != 0;
                            let num = (GetKeyState(0x90) & 1) != 0;
                            if last_caps_lock != Some(caps) || last_num_lock != Some(num) {
                                let _ = input_tx.send(ControlEvent::KeyLockState { caps_lock: caps, num_lock: num });
                                last_caps_lock = Some(caps);
                                last_num_lock = Some(num);
                            }
                        }
                    }
                }

                if relative_mouse_mode {
                    let (win_w, win_h) = r.window.get_size();
                    let center_x = win_w as f32 / 2.0;
                    let center_y = win_h as f32 / 2.0;
                    if let Some((mx, my)) = r.window.get_mouse_pos(minifb::MouseMode::Pass) {
                        let dx = (mx - center_x) as i32;
                        let dy = (my - center_y) as i32;
                        if dx != 0 || dy != 0 {
                            let _ = input_tx.send(ControlEvent::MouseMoveRelative { dx, dy });
                            let (win_left, win_top) = r.window.get_position();
                            let screen_center_x = win_left as i32 + (win_w as i32 / 2);
                            let screen_center_y = win_top as i32 + (win_h as i32 / 2);
                            #[cfg(windows)]
                            unsafe {
                                let _ = SetCursorPos(screen_center_x, screen_center_y);
                            }
                        }
                    }
                } else {
                    if let Some((mx, my)) = r.window.get_mouse_pos(minifb::MouseMode::Discard) {
                        let current_pos = (mx, my);
                        if last_mouse_pos != Some(current_pos) {
                            let is_at_edge = mx <= 2.0 || mx >= (r.width as f32 - 3.0) || my <= 2.0 || my >= (r.height as f32 - 3.0);
                            if is_at_edge {
                                if edge_touch_start.is_none() {
                                    edge_touch_start = Some(std::time::Instant::now());
                                } else if edge_touch_start.unwrap().elapsed() >= std::time::Duration::from_millis(150) {
                                    let _ = input_tx.send(ControlEvent::MouseMove { dx: mx as i32, dy: my as i32 });
                                    last_mouse_pos = Some(current_pos);
                                }
                            } else {
                                edge_touch_start = None;
                                let _ = input_tx.send(ControlEvent::MouseMove { dx: mx as i32, dy: my as i32 });
                                last_mouse_pos = Some(current_pos);
                            }
                        }
                    }
                }

                let left_down = r.window.get_mouse_down(minifb::MouseButton::Left);
                if left_down != last_left_down {
                    let _ = input_tx.send(ControlEvent::MouseButton { button: "left".to_string(), pressed: left_down });
                    last_left_down = left_down;
                }
                let right_down = r.window.get_mouse_down(minifb::MouseButton::Right);
                if right_down != last_right_down {
                    let _ = input_tx.send(ControlEvent::MouseButton { button: "right".to_string(), pressed: right_down });
                    last_right_down = right_down;
                }
                let middle_down = r.window.get_mouse_down(minifb::MouseButton::Middle);
                if middle_down != last_middle_down {
                    let _ = input_tx.send(ControlEvent::MouseButton { button: "middle".to_string(), pressed: middle_down });
                    last_middle_down = middle_down;
                }

                let current_keys = r.window.get_keys();
                let current_keys_set: std::collections::HashSet<minifb::Key> = current_keys.iter().cloned().collect();
                for k in &current_keys_set {
                    if !last_keys.contains(k) {
                        let _ = input_tx.send(ControlEvent::Key { key: format!("{:?}", k), pressed: true });
                    }
                }
                for k in &last_keys {
                    if !current_keys_set.contains(k) {
                        let _ = input_tx.send(ControlEvent::Key { key: format!("{:?}", k), pressed: false });
                    }
                }
                last_keys = current_keys_set;

                let mut char_keys = Vec::new();
                if let Ok(mut guard) = r.char_buffer.lock() {
                    char_keys = std::mem::take(&mut *guard);
                }
                for c in char_keys {
                    if !c.is_control() {
                        let _ = input_tx.send(ControlEvent::UnicodeChar { ch: c.to_string() });
                    }
                }
            }

            let elapsed = tick_start.elapsed();
            if elapsed < std::time::Duration::from_millis(8) {
                std::thread::sleep(std::time::Duration::from_millis(8) - elapsed);
            }
        }
    });

    let render_tx_clone = render_tx.clone();
    let handle_disconnect = |logger: &Logger, render_tx: &std::sync::mpsc::SyncSender<RenderJob>| -> Result<(TcpStream, TcpStream, BufReader<TcpStream>), String> {
        let dummy_job = RenderJob {
            buffer: vec![0; 320 * 240],
            width: 320,
            height: 240,
            title: "[RECONECTANDO...] LanPilot Agent Stream".to_string(),
            compat_mode: true,
        };
        let _ = render_tx.send(dummy_job);
        logger.log("Conexión perdida. Iniciando reconexión adaptativa automática de 15 segundos...".to_string());
        let start_reconnect = std::time::Instant::now();
        loop {
            if is_stopped(stop) {
                return Err("Reconexión cancelada por el usuario.".to_string());
            }
            if start_reconnect.elapsed() >= std::time::Duration::from_secs(15) {
                return Err("Tiempo de reconexión agotado (15 segundos).".to_string());
            }
            
            let stream_endpoint = format!("{}:{}", host_ipv4, ack.stream_port);
            let control_endpoint = format!("{}:{}", host_ipv4, ack.control_port);
            
            if let Ok(new_stream) = TcpStream::connect(stream_endpoint.as_str()) {
                if let Ok(new_control) = TcpStream::connect(control_endpoint.as_str()) {
                    let _ = new_stream.set_nodelay(true);
                    let _ = new_stream.set_read_timeout(Some(Duration::from_secs(2)));
                    
                    let _ = new_control.set_nodelay(true);
                    let _ = new_control.set_write_timeout(Some(Duration::from_secs(5)));
                    
                    let hello = StreamHello::new(ack.session_id.clone(), agent_name.to_string());
                    if let Ok(hello_line) = to_json_line(&hello) {
                        let mut temp_stream = new_stream;
                        if temp_stream.write_all(hello_line.as_bytes()).is_ok() {
                            logger.log("Conexión restablecida con éxito de forma transparente.".to_string());
                            let new_reader = BufReader::new(temp_stream.try_clone().unwrap());
                            return Ok((temp_stream, new_control, new_reader));
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    };

    let send_control_event = {
        let tx_mutex = Arc::clone(&control_tx_mutex);
        use std::io::Write;
        move |frame: &lanpilot_core::ControlFrame| {
            if let Ok(line) = to_json_line(frame) {
                let mut guard = tx_mutex.lock().unwrap();
                let encrypted = lanpilot_core::encrypt_line(&line, &mut guard.1);
                let _ = guard.0.write_all(encrypted.as_bytes());
            }
        }
    };

    while target_frames == 0 || received < target_frames {
        if is_stopped(stop) {
            logger.log("Transmisión cancelada por el usuario.".to_string());
            break;
        }

        while let Ok(event) = input_rx.try_recv() {
            match event {
                ControlEvent::TogglePrivacyMode { .. } => {
                    privacy_mode_active = !privacy_mode_active;
                    logger.log(format!("Solicitando alternar Modo Privacidad en Host (Activo={}).", privacy_mode_active));
                    let ev = ControlEvent::TogglePrivacyMode { enabled: privacy_mode_active };
                    let control_frame = ControlFrame::new(ack.session_id.clone(), vec![ev]);
                    send_control_event(&control_frame);
                }
                ControlEvent::SetVideoFormat { .. } => {
                    use_rgb565 = !use_rgb565;
                    logger.log(format!("Solicitando cambio de formato de video (use_rgb565={}).", use_rgb565));
                    let ev = ControlEvent::SetVideoFormat { use_rgb565 };
                    let control_frame = ControlFrame::new(ack.session_id.clone(), vec![ev]);
                    send_control_event(&control_frame);
                }
                ControlEvent::FileChunk { filename, .. } if filename == "trigger_select_file" => {
                    logger.log("Abriendo selector de archivos para transferencia remota (Ctrl + Alt + F)...".to_string());
                    let control_stream_clone = control_stream.try_clone().ok();
                    let session_id = ack.session_id.clone();
                    let logger_thread = logger.clone();
                    let tx_mutex_clone = Arc::clone(&control_tx_mutex);
                    let pair_code_clone = config.pair_code.clone();
                    std::thread::spawn(move || {
                        let Some(stream) = control_stream_clone else { return; };
                        let mut reader_stream = match stream.try_clone() {
                            Ok(s) => std::io::BufReader::new(s),
                            Err(_) => return,
                        };
                        
                        let script = "[System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms') | Out-Null; \
                                      $o = New-Object System.Windows.Forms.OpenFileDialog; \
                                      $o.Filter = 'Todos los archivos (*.*)|*.*'; \
                                      $o.Title = 'Selecciona archivo para enviar al Host'; \
                                      if($o.ShowDialog() -eq 'OK') { $o.FileName }";
                        let output = std::process::Command::new("powershell")
                            .args(&["-NoProfile", "-Command", script])
                            .output();
                        if let Ok(res) = output {
                            let path_str = String::from_utf8_lossy(&res.stdout).trim().to_string();
                            if path_str.is_empty() { return; }
                            let path = std::path::Path::new(&path_str);
                            if !path.exists() { return; }
                            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("archivo").to_string();
                            let total_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                            logger_thread.log(format!("Iniciando transferencia asíncrona de '{}' ({} bytes)", filename, total_size));
                            if let Ok(mut file) = std::fs::File::open(&path) {
                                use std::io::Read;
                                use std::io::Write;
                                use std::io::BufRead;
                                use std::io::Seek;
                                
                                let mut buffer = vec![0u8; 32768];
                                let mut offset = 0_u64;
                                let mut acked_offset = 0_u64;
                                let mut in_flight = std::collections::VecDeque::new();
                                const WINDOW_SIZE: usize = 6;
                                
                                let _ = reader_stream.get_ref().set_read_timeout(Some(std::time::Duration::from_secs(2)));
                                let mut file_cipher_rx = lanpilot_core::Rc4Cipher::new(format!("{}-control-h2c", pair_code_clone).as_bytes());

                                loop {
                                    while in_flight.len() < WINDOW_SIZE {
                                        match file.read(&mut buffer) {
                                            Ok(0) => break,
                                            Ok(n) => {
                                                let chunk_b64 = BASE64.encode(&buffer[0..n]);
                                                let event = ControlEvent::FileChunk {
                                                    filename: filename.clone(),
                                                    offset,
                                                    total_size,
                                                    data_b64: chunk_b64,
                                                };
                                                let control_frame = ControlFrame::new(session_id.clone(), vec![event]);
                                                if let Ok(line) = to_json_line(&control_frame) {
                                                    let mut guard = tx_mutex_clone.lock().unwrap();
                                                    let encrypted = lanpilot_core::encrypt_line(&line, &mut guard.1);
                                                    let _ = guard.0.write_all(encrypted.as_bytes());
                                                }
                                                std::thread::sleep(std::time::Duration::from_millis(5));
                                                in_flight.push_back((offset, n as u64));
                                                offset += n as u64;
                                            }
                                            Err(_) => return,
                                        }
                                    }

                                    if in_flight.is_empty() {
                                        break;
                                    }

                                    let mut ack_line = String::new();
                                    match reader_stream.read_line(&mut ack_line) {
                                        Ok(0) => {
                                            logger_thread.log("Conexión de transferencia cerrada.".to_string());
                                            return;
                                        }
                                        Ok(_) => {
                                            if let Ok(decrypted) = lanpilot_core::decrypt_line(&ack_line, &mut file_cipher_rx) {
                                                if let Ok(frame) = from_json_line::<ControlFrame>(&decrypted) {
                                                    for ev in frame.events {
                                                        if let ControlEvent::FileChunkAck { offset: ack_offset, .. } = ev {
                                                            while let Some(&(off, size)) = in_flight.front() {
                                                                if off <= ack_offset {
                                                                    acked_offset = off + size;
                                                                    in_flight.pop_front();
                                                                } else {
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                                            if let Some(&(first_off, _)) = in_flight.front() {
                                                logger_thread.log(format!("Re-sincronizando ventana de transferencia desde offset {}...", first_off));
                                            }
                                            in_flight.clear();
                                            offset = acked_offset;
                                            let _ = file.seek(std::io::SeekFrom::Start(offset));
                                        }
                                        Err(_) => return,
                                    }
                                }
                                logger_thread.log(format!("Transferencia asíncrona de '{}' finalizada con éxito.", filename));
                            }
                        }
                    });
                }
                other_event => {
                    let control_frame = ControlFrame::new(ack.session_id.clone(), vec![other_event]);
                    if let Ok(line) = to_json_line(&control_frame) {
                        let _ = control_stream.write_all(line.as_bytes());
                    }
                }
            }
        }

        let mut line = String::new();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                if last_frame_received_at.elapsed() >= std::time::Duration::from_secs(6) {
                    logger.log("Keep-alive timeout expirado. Intentando reconectar...".to_string());
                    match handle_disconnect(&logger, &render_tx_clone) {
                        Ok((new_s, new_c, new_r)) => {
                            stream = new_s;
                            control_stream = new_c;
                            reader = new_r;
                            last_frame_received_at = std::time::Instant::now();
                            timeout_streak = 0;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                timeout_streak += 1;
                if timeout_streak >= MAX_CONSECUTIVE_STREAM_TIMEOUTS {
                    logger.log("Múltiples timeouts consecutivos. Intentando reconectar...".to_string());
                    match handle_disconnect(&logger, &render_tx_clone) {
                        Ok((new_s, new_c, new_r)) => {
                            stream = new_s;
                            control_stream = new_c;
                            reader = new_r;
                            last_frame_received_at = std::time::Instant::now();
                            timeout_streak = 0;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                continue;
            }
            Err(err) => {
                logger.log(format!("Error de lectura en socket: {err}. Intentando reconectar..."));
                match handle_disconnect(&logger, &render_tx_clone) {
                    Ok((new_s, new_c, new_r)) => {
                        stream = new_s;
                        control_stream = new_c;
                        reader = new_r;
                        
                        stream_cipher_rx = lanpilot_core::Rc4Cipher::new(format!("{}-stream-h2c", config.pair_code).as_bytes());
                        stream_cipher_tx = lanpilot_core::Rc4Cipher::new(format!("{}-stream-c2h", config.pair_code).as_bytes());
                        let new_control_tx = lanpilot_core::Rc4Cipher::new(format!("{}-control-c2h", config.pair_code).as_bytes());
                        if let Ok(mut guard) = control_tx_mutex.lock() {
                            guard.0 = control_stream.try_clone().unwrap();
                            guard.1 = new_control_tx;
                        }

                        last_frame_received_at = std::time::Instant::now();
                        timeout_streak = 0;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        if bytes_read == 0 {
            logger.log("Socket de red cerrado de golpe. Intentando reconectar...".to_string());
            match handle_disconnect(&logger, &render_tx_clone) {
                Ok((new_s, new_c, new_r)) => {
                    stream = new_s;
                    control_stream = new_c;
                    reader = new_r;
                    
                    stream_cipher_rx = lanpilot_core::Rc4Cipher::new(format!("{}-stream-h2c", config.pair_code).as_bytes());
                    stream_cipher_tx = lanpilot_core::Rc4Cipher::new(format!("{}-stream-c2h", config.pair_code).as_bytes());
                    let new_control_tx = lanpilot_core::Rc4Cipher::new(format!("{}-control-c2h", config.pair_code).as_bytes());
                    if let Ok(mut guard) = control_tx_mutex.lock() {
                        guard.0 = control_stream.try_clone().unwrap();
                        guard.1 = new_control_tx;
                    }

                    last_frame_received_at = std::time::Instant::now();
                    timeout_streak = 0;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        last_frame_received_at = std::time::Instant::now();
        timeout_streak = 0;

        let decrypted_line = match lanpilot_core::decrypt_line(&line, &mut stream_cipher_rx) {
            Ok(d) => d,
            Err(err) => {
                logger.log(format!("stream decrypt error: {err}"));
                continue;
            }
        };

        let frame: StreamFrame =
            from_json_line(&decrypted_line).map_err(|err| format!("decode stream frame failed: {err}"))?;
        if frame.magic != PROTOCOL_MAGIC || frame.session_id != ack.session_id {
            return Err(format!("invalid stream frame payload: {:?}", frame));
        }

        let now_ms = unix_timestamp_ms();
        if frame.captured_at_ms > 0 && now_ms >= frame.captured_at_ms {
            let frame_latency = now_ms.saturating_sub(frame.captured_at_ms);
            if last_quality_adjustment.elapsed() >= std::time::Duration::from_secs(2) {
                if frame_latency > 100 {
                    let _ = input_tx.send(ControlEvent::ReduceQuality);
                    last_quality_adjustment = std::time::Instant::now();
                } else if frame_latency < 30 {
                    let _ = input_tx.send(ControlEvent::IncreaseQuality);
                    last_quality_adjustment = std::time::Instant::now();
                }
            }
        }

        if frame.source == "clipboard_rich" {
            if let Ok(compressed) = BASE64.decode(&frame.compressed_payload_b64) {
                if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed) {
                    let fmt_name = if frame.pixel_format == "html" { "HTML Format" } else { "Rich Text Format" };
                    #[cfg(windows)]
                    {
                        let _ = lanpilot_core::write_rich_clipboard(fmt_name, &decompressed);
                        let hash = {
                            let mut h = decompressed.len() as u64;
                            if !decompressed.is_empty() {
                                h = h.wrapping_add(decompressed[0] as u64);
                                h = h.wrapping_add(decompressed[decompressed.len() / 2] as u64);
                                h = h.wrapping_add(decompressed[decompressed.len() - 1] as u64);
                            }
                            h
                        };
                        if fmt_name == "HTML Format" {
                            last_html_hash = Some(hash);
                        } else {
                            last_rtf_hash = Some(hash);
                        }
                        logger.log(format!("Portapapeles rico '{}' sincronizado desde host remoto: {} bytes", fmt_name, decompressed.len()));
                    }
                }
            }
            continue;
        }

        if frame.source == "clipboard" {
            if let Ok(compressed) = BASE64.decode(&frame.compressed_payload_b64) {
                if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed) {
                    if let Ok(text) = String::from_utf8(decompressed) {
                        let text_trimmed = text.trim().to_string();
                        if !text_trimmed.is_empty() {
                            if let Some(ref mut cb) = local_clipboard {
                                let _ = cb.set_text(text_trimmed.clone());
                                last_sent_clipboard = text_trimmed;
                                logger.log(format!("Portapapeles sincronizado desde host remoto: {} bytes", last_sent_clipboard.len()));
                            }
                        }
                    }
                }
            }
            continue;
        }

        if frame.source == "ping" {
            let response = ControlEvent::Pong { timestamp_ms: frame.captured_at_ms as u64 };
            let control_frame = ControlFrame::new(ack.session_id.clone(), vec![response]);
            send_control_event(&control_frame);
            continue;
        }

        if frame.source == "clipboard_image" {
            if let Ok(compressed) = BASE64.decode(&frame.compressed_payload_b64) {
                if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed) {
                    if let Some(ref mut cb) = local_clipboard {
                        let img_data = arboard::ImageData {
                            width: frame.width as usize,
                            height: frame.height as usize,
                            bytes: std::borrow::Cow::Owned(decompressed),
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
                        logger.log(format!("Portapapeles de imagen sincronizado desde host remoto: {}x{}", frame.width, frame.height));
                    }
                }
            }
            continue;
        }

        last_captured_at_ms = frame.captured_at_ms as u64;
        let compat_mode = frame.source.eq_ignore_ascii_case("synthetic");
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
            let width = frame.width as usize;
            let height = frame.height as usize;
            let stride_bytes = frame.stride_bytes;
            let is_rgb565 = frame.pixel_format == "rgb565";

            let mut draw_buf = match buffer_pool_rx.try_recv() {
                Ok(buf) => buf,
                Err(_) => vec![0; width * height],
            };
            if draw_buf.len() != width * height {
                draw_buf.resize(width * height, 0);
            }

            if let Some(ref tiles) = frame.tiles {
                for tile in tiles {
                    if let Ok(compressed) = BASE64.decode(tile.compressed_payload_b64.as_bytes()) {
                        if let Ok(decompressed) = decompress_size_prepended(&compressed) {
                            let tile_row_stride = tile.width as usize * (if is_rgb565 { 2 } else { 4 });
                            for ty in 0..tile.height as usize {
                                let gy = tile.y as usize + ty;
                                if gy >= height { continue; }
                                for tx in 0..tile.width as usize {
                                    let gx = tile.x as usize + tx;
                                    if gx >= width { continue; }
                                    
                                    if is_rgb565 {
                                        let idx = ty * tile_row_stride + tx * 2;
                                        if idx + 1 < decompressed.len() {
                                            let val = (decompressed[idx] as u16) | ((decompressed[idx + 1] as u16) << 8);
                                            let r5 = (val >> 11) & 0x1F;
                                            let g6 = (val >> 5) & 0x3F;
                                            let b5 = val & 0x1F;
                                            let r = ((r5 * 255) / 31) as u32;
                                            let g = ((g6 * 255) / 63) as u32;
                                            let b = ((b5 * 255) / 31) as u32;
                                            draw_buf[gy * width + gx] = (r << 16) | (g << 8) | b;
                                        }
                                    } else {
                                        let idx = ty * tile_row_stride + tx * 4;
                                        if idx + 2 < decompressed.len() {
                                            let b = decompressed[idx] as u32;
                                            let g = decompressed[idx + 1] as u32;
                                            let r = decompressed[idx + 2] as u32;
                                            draw_buf[gy * width + gx] = (r << 16) | (g << 8) | b;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                if is_rgb565 {
                    for y in 0..height {
                        let row_start = y * stride_bytes;
                        for x in 0..width {
                            let idx = row_start + x * 2;
                            if idx + 1 < raw.len() {
                                let val = (raw[idx] as u16) | ((raw[idx + 1] as u16) << 8);
                                let r5 = (val >> 11) & 0x1F;
                                let g6 = (val >> 5) & 0x3F;
                                let b5 = val & 0x1F;
                                let r = ((r5 * 255) / 31) as u32;
                                let g = ((g6 * 255) / 63) as u32;
                                let b = ((b5 * 255) / 31) as u32;
                                draw_buf[y * width + x] = (r << 16) | (g << 8) | b;
                            }
                        }
                    }
                } else {
                    if stride_bytes == width * 4 && raw.len() >= width * height * 4 {
                        unsafe {
                            let src_u32 = std::slice::from_raw_parts(raw.as_ptr() as *const u32, width * height);
                            draw_buf.copy_from_slice(src_u32);
                        }
                    } else {
                        for y in 0..height {
                            let row_start = y * stride_bytes;
                            for x in 0..width {
                                let idx = row_start + x * 4;
                                if idx + 2 < raw.len() {
                                    let b = raw[idx] as u32;
                                    let g = raw[idx + 1] as u32;
                                    let r = raw[idx + 2] as u32;
                                    draw_buf[y * width + x] = (r << 16) | (g << 8) | b;
                                }
                            }
                        }
                    }
                }
            }

            let summary = metrics.summary();
            let quality = if summary.avg_latency_ms < 20.0 {
                "[Excelente 🟢]"
            } else if summary.avg_latency_ms <= 60.0 {
                "[Buena 🟡]"
            } else {
                "[Alerta de Red 🔴]"
            };
            let privacy_suffix = if privacy_mode_active { " [PRIVADO 🕶️]" } else { "" };
            let title = if compat_mode {
                format!("LanPilot Agent Stream — {} — Modo compatibilidad ({:.1} FPS — Latencia: {:.0}ms){}", quality, summary.fps, summary.avg_latency_ms, privacy_suffix)
            } else {
                format!("LanPilot Agent Stream — {} — {:.1} FPS — Latencia: {:.0}ms{}", quality, summary.fps, summary.avg_latency_ms, privacy_suffix)
            };

            let job = RenderJob {
                buffer: draw_buf,
                width,
                height,
                title,
                compat_mode,
            };
            let _ = render_tx.send(job);
        }

        received += 1;
        last_sequence = frame.sequence;
        total_raw += raw.len();
        metrics.observe(frame.captured_at_ms, frame.frame_interval_ms);

        if received % 10 == 0 {
            let summary = metrics.summary();
            let (target_fps, scale_divisor) = choose_adaptive_target(&summary);
            let echo = last_captured_at_ms;
            if last_feedback != Some((target_fps, scale_divisor)) {
                last_feedback = Some((target_fps, scale_divisor));
                let _ = send_stream_feedback(
                    &control_tx_mutex,
                    ack,
                    &summary,
                    target_fps,
                    scale_divisor,
                    echo,
                );
            }

            if let Some(ref mut cb) = local_clipboard {
                if let Ok(text) = cb.get_text() {
                    let text_trimmed = text.trim().to_string();
                    if !text_trimmed.is_empty() && text_trimmed != last_sent_clipboard {
                        let clipboard_frame = ControlFrame::new(
                            ack.session_id.clone(),
                            vec![ControlEvent::Clipboard { text: text_trimmed.clone() }],
                        );
                        send_control_event(&clipboard_frame);
                        last_sent_clipboard = text_trimmed;
                    }
                }
                
                let now_time = std::time::Instant::now();
                if now_time.duration_since(last_clipboard_image_check) >= std::time::Duration::from_millis(1500) {
                    last_clipboard_image_check = now_time;
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
                            let encoded = BASE64.encode(&img.bytes);
                            let clipboard_img_frame = ControlFrame::new(
                                ack.session_id.clone(),
                                vec![ControlEvent::ClipboardImage {
                                    width: img.width,
                                    height: img.height,
                                    rgba_payload_b64: encoded,
                                }],
                            );
                            send_control_event(&clipboard_img_frame);
                        }
                    }

                    // Sincronización de Portapapeles Enriquecido (HTML y RTF)
                    #[cfg(windows)]
                    {
                        for fmt in &["HTML Format", "Rich Text Format"] {
                            if let Some(data) = lanpilot_core::read_rich_clipboard(fmt) {
                                let hash = {
                                    let mut h = data.len() as u64;
                                    if !data.is_empty() {
                                        h = h.wrapping_add(data[0] as u64);
                                        h = h.wrapping_add(data[data.len() / 2] as u64);
                                        h = h.wrapping_add(data[data.len() - 1] as u64);
                                    }
                                    h
                                };
                                let is_new = match fmt {
                                    &"HTML Format" => last_html_hash != Some(hash),
                                    &"Rich Text Format" => last_rtf_hash != Some(hash),
                                    _ => false,
                                };
                                if is_new {
                                    if fmt == &"HTML Format" { last_html_hash = Some(hash); }
                                    else { last_rtf_hash = Some(hash); }
                                    
                                    let encoded = BASE64.encode(&data);
                                    let rich_frame = ControlFrame::new(
                                        ack.session_id.clone(),
                                        vec![ControlEvent::ClipboardRichText {
                                            format: fmt.to_string(),
                                            payload_b64: encoded,
                                        }],
                                    );
                                    send_control_event(&rich_frame);
                                }
                            }
                        }
                    }
                }
            }
        }
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
    if summary.avg_latency_ms > 200.0 || summary.jitter_ms > 100.0 {
        (15, 3) // Conexión muy lenta / VPN: 15 FPS, escala 1/3 (ancho de banda ultra bajo)
    } else if summary.avg_latency_ms > 80.0 || summary.jitter_ms > 40.0 {
        (24, 2) // Conexión media / Wi-Fi inestable: 24 FPS, escala 1/2 (balanceado)
    } else {
        (60, 1) // Conexión óptima / LAN cableada: 60 FPS, calidad nativa cristalina (ultra-smooth)
    }
}

fn send_stream_feedback(
    tx_mutex: &Arc<std::sync::Mutex<(TcpStream, lanpilot_core::Rc4Cipher)>>,
    ack: &HandshakeAck,
    summary: &StreamSummary,
    target_fps: u32,
    scale_divisor: u8,
    echo_captured_at_ms: u64,
) -> Result<(), String> {
    let feedback = ControlFrame::new(
        ack.session_id.clone(),
        vec![ControlEvent::StreamFeedback {
            target_fps,
            scale_divisor,
            avg_latency_ms: summary.avg_latency_ms.round() as u32,
            jitter_ms: summary.jitter_ms.round() as u32,
            echo_captured_at_ms,
        }],
    );
    let encoded =
        to_json_line(&feedback).map_err(|err| format!("encode feedback frame failed: {err}"))?;
    
    let mut guard = tx_mutex.lock().unwrap();
    let encrypted = lanpilot_core::encrypt_line(&encoded, &mut guard.1);
    use std::io::Write;
    guard.0
        .write_all(encrypted.as_bytes())
        .map_err(|err| format!("send feedback frame failed: {err}"))?;
    Ok(())
}

fn decode_stream_frame(frame: &StreamFrame) -> Result<Vec<u8>, String> {
    if frame.tiles.is_some() {
        return Ok(Vec::new());
    }
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

struct UnicodeCollector {
    chars: std::sync::Arc<std::sync::Mutex<Vec<char>>>,
}

impl minifb::InputCallback for UnicodeCollector {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(c) = std::char::from_u32(uni_char) {
            if let Ok(mut guard) = self.chars.lock() {
                guard.push(c);
            }
        }
    }
}

struct FrameRenderer {
    window: Window,
    width: usize,
    height: usize,
    buffer: Vec<u32>,
    last_compat_mode: Option<bool>,
    is_fullscreen: bool,
    stream_width: usize,
    stream_height: usize,
    char_buffer: std::sync::Arc<std::sync::Mutex<Vec<char>>>,
}

impl FrameRenderer {
    fn try_new(width: usize, height: usize) -> Result<Self, String> {
        let options = WindowOptions {
            resize: true,
            scale: Scale::X1,
            scale_mode: minifb::ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        };
        let mut window = Window::new("LanPilot Agent Stream", width, height, options)
            .map_err(|err| format!("create render window failed: {err}"))?;
        window.set_target_fps(60);

        let char_buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        window.set_input_callback(Box::new(UnicodeCollector {
            chars: std::sync::Arc::clone(&char_buffer),
        }));

        Ok(Self {
            window,
            width,
            height,
            buffer: vec![0_u32; width * height],
            last_compat_mode: None,
            is_fullscreen: false,
            stream_width: width,
            stream_height: height,
            char_buffer,
        })
    }

    fn toggle_fullscreen(&mut self) -> Result<(), String> {
        self.is_fullscreen = !self.is_fullscreen;
        
        let (win_width, win_height) = if self.is_fullscreen {
            #[cfg(windows)]
            {
                unsafe {
                    let w = GetSystemMetrics(0); // SM_CXSCREEN = 0
                    let h = GetSystemMetrics(1); // SM_CYSCREEN = 1
                    if w > 0 && h > 0 {
                        (w as usize, h as usize)
                    } else {
                        (self.stream_width, self.stream_height)
                    }
                }
            }
            #[cfg(not(windows))]
            (self.stream_width, self.stream_height)
        } else {
            (self.stream_width, self.stream_height)
        };
        
        let options = WindowOptions {
            resize: !self.is_fullscreen,
            scale: Scale::X1,
            scale_mode: minifb::ScaleMode::AspectRatioStretch,
            borderless: self.is_fullscreen,
            topmost: self.is_fullscreen,
            ..WindowOptions::default()
        };
        
        let mut new_window = Window::new("LanPilot Agent Stream", win_width, win_height, options)
            .map_err(|err| format!("create render window failed: {err}"))?;
        new_window.set_target_fps(60);
        
        if let Some(compat) = self.last_compat_mode {
            new_window.set_title(if compat {
                "LanPilot Agent Stream — Modo compatibilidad"
            } else {
                "LanPilot Agent Stream — Captura real"
            });
        }
        
        self.window = new_window;
        Ok(())
    }

    #[allow(dead_code)]
    fn render(&mut self, frame: &StreamFrame, raw: &[u8], compat_mode: bool) -> Result<(), String> {
        let width = frame.width as usize;
        let height = frame.height as usize;
        let stride_bytes = frame.stride_bytes;
        let is_rgb565 = frame.pixel_format == "rgb565";
        
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

        if let Some(ref tiles) = frame.tiles {
            for tile in tiles {
                let compressed = BASE64.decode(tile.compressed_payload_b64.as_bytes())
                    .map_err(|err| format!("base64 decode tile failed: {err}"))?;
                let decompressed = decompress_size_prepended(&compressed)
                    .map_err(|err| format!("lz4 decompress tile failed: {err}"))?;
                
                let tile_row_stride = tile.width as usize * (if is_rgb565 { 2 } else { 4 });
                for ty in 0..tile.height as usize {
                    let gy = tile.y as usize + ty;
                    if gy >= height { continue; }
                    for tx in 0..tile.width as usize {
                        let gx = tile.x as usize + tx;
                        if gx >= width { continue; }
                        
                        if is_rgb565 {
                            let idx = ty * tile_row_stride + tx * 2;
                            if idx + 1 < decompressed.len() {
                                let val = (decompressed[idx] as u16) | ((decompressed[idx + 1] as u16) << 8);
                                let r5 = (val >> 11) & 0x1F;
                                let g6 = (val >> 5) & 0x3F;
                                let b5 = val & 0x1F;
                                let r = ((r5 * 255) / 31) as u32;
                                let g = ((g6 * 255) / 63) as u32;
                                let b = ((b5 * 255) / 31) as u32;
                                self.buffer[gy * width + gx] = (r << 16) | (g << 8) | b;
                            }
                        } else {
                            let idx = ty * tile_row_stride + tx * 4;
                            if idx + 2 < decompressed.len() {
                                let b = decompressed[idx] as u32;
                                let g = decompressed[idx + 1] as u32;
                                let r = decompressed[idx + 2] as u32;
                                self.buffer[gy * width + gx] = (r << 16) | (g << 8) | b;
                            }
                        }
                    }
                }
            }
        } else {
            let expected_stride = if is_rgb565 { width * 2 } else { width * 4 };
            if stride_bytes < expected_stride {
                return Err(format!(
                    "invalid stride {} (expected at least {})",
                    stride_bytes,
                    expected_stride
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

            if is_rgb565 {
                for y in 0..height {
                    let row_start = y * stride_bytes;
                    for x in 0..width {
                        let idx = row_start + x * 2;
                        let val = (raw[idx] as u16) | ((raw[idx + 1] as u16) << 8);
                        let r5 = (val >> 11) & 0x1F;
                        let g6 = (val >> 5) & 0x3F;
                        let b5 = val & 0x1F;
                        let r = ((r5 * 255) / 31) as u32;
                        let g = ((g6 * 255) / 63) as u32;
                        let b = ((b5 * 255) / 31) as u32;
                        self.buffer[y * width + x] = (r << 16) | (g << 8) | b;
                    }
                }
            } else {
                if stride_bytes == width * 4 {
                    unsafe {
                        let src_u32 = std::slice::from_raw_parts(raw.as_ptr() as *const u32, width * height);
                        self.buffer.copy_from_slice(src_u32);
                    }
                } else {
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
                }
            }
        }

        self.window
            .update_with_buffer(&self.buffer, width, height)
            .map_err(|err| format!("render update failed: {err}"))
    }

    #[allow(dead_code)]
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

fn spawn_audio_client(host_ipv4: String, stop: StopFlag, logger: Logger, pair_code: String) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::collections::VecDeque;

    std::thread::spawn(move || {
        let endpoint = format!("{}:{}", host_ipv4, AUDIO_PORT);
        logger.log(format!("Conectando al stream de audio en {}...", endpoint));
        
        let mut stream = match TcpStream::connect(&endpoint) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                s
            }
            Err(e) => {
                logger.log(format!("Aviso: no se pudo conectar al servidor de audio: {e}"));
                return;
            }
        };
        
        let mut header = [0u8; 8];
        if let Err(e) = std::io::Read::read_exact(&mut stream, &mut header) {
            logger.log(format!("Aviso: error al leer cabecera de audio: {e}"));
            return;
        }
        
        let sample_rate = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let channels = u16::from_le_bytes([header[4], header[5]]);
        let use_compression = u16::from_le_bytes([header[6], header[7]]) == 1;
        
        logger.log(format!(
            "Stream de audio inicializado: {} Hz, {} canales (PCM 16-bit, compression={})",
            sample_rate, channels, use_compression
        ));
        
        #[cfg(windows)]
        unsafe {
            #[link(name = "ole32")]
            unsafe extern "system" {
                fn CoInitializeEx(pv_reserved: *mut std::ffi::c_void, dw_co_init: u32) -> i32;
            }
            let _ = CoInitializeEx(std::ptr::null_mut(), 0x2);
        }
        
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                logger.log("Aviso: no se encontró dispositivo de salida de audio".to_string());
                return;
            }
        };
        
        let output_config = match device.default_output_config() {
            Ok(c) => c,
            Err(e) => {
                logger.log(format!("Aviso: no se pudo obtener la configuración de salida de audio: {e}"));
                return;
            }
        };
        let agent_config: cpal::StreamConfig = output_config.clone().into();
        let agent_sample_rate = agent_config.sample_rate.0 as f64;
        let agent_channels = agent_config.channels as usize;
        
        let host_sample_rate = sample_rate as f64;
        let host_channels = channels as usize;
        
        logger.log(format!(
            "Tarjeta local de audio: {} Hz, {} canales",
            agent_config.sample_rate.0, agent_config.channels
        ));
        
        let audio_buffer = Arc::new(Mutex::new(VecDeque::<i16>::new()));
        let play_buffer = Arc::clone(&audio_buffer);
        
        // Estado del resampler lineal y Jitter Buffer adaptativo
        let mut last_frame = vec![0.0f32; host_channels];
        let mut next_frame = vec![0.0f32; host_channels];
        let mut phase = 0.0f64;
        let base_factor = host_sample_rate / agent_sample_rate;
        let target_latency_samples = (host_sample_rate * host_channels as f64 * 0.08) as usize; // 80ms target
        let threshold = (host_sample_rate * host_channels as f64 * 0.02) as usize; // 20ms threshold
        let mut fade_volume = 1.0f32;
        
        let cpal_stream = match device.build_output_stream(
            &agent_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut buffer = play_buffer.lock().unwrap();
                let current_len = buffer.len();
                
                // Modular la velocidad de resampler para sincronizar la latencia
                let speed_multiplier = if current_len > target_latency_samples + threshold {
                    1.02f64 // Consumir buffer un 2% más rápido
                } else if current_len > 0 && current_len < target_latency_samples.saturating_sub(threshold) {
                    0.98f64 // Estirar el buffer un 2% más lento
                } else {
                    1.00f64
                };
                let current_factor = base_factor * speed_multiplier;
                
                for frame in data.chunks_exact_mut(agent_channels) {
                    while phase >= 1.0 {
                        last_frame.copy_from_slice(&next_frame);
                        if buffer.len() >= host_channels {
                            for i in 0..host_channels {
                                if let Some(s) = buffer.pop_front() {
                                    next_frame[i] = (s as f32) / 32768.0;
                                }
                            }
                            phase -= 1.0;
                            fade_volume = (fade_volume + 0.05).min(1.0);
                        } else {
                            fade_volume = (fade_volume - 0.02).max(0.0);
                            if fade_volume == 0.0 {
                                next_frame.fill(0.0);
                                phase = 0.0;
                                break;
                            } else {
                                for i in 0..host_channels {
                                    next_frame[i] = next_frame[i] * fade_volume;
                                }
                                phase -= 1.0;
                            }
                        }
                    }
                    
                    let t = phase as f32;
                    let interpolated_channel = |ch: usize| -> f32 {
                        if ch < host_channels {
                            ((1.0 - t) * last_frame[ch] + t * next_frame[ch]) * fade_volume
                        } else {
                            0.0
                        }
                    };
                    
                    for i in 0..agent_channels {
                        if host_channels == 1 {
                            frame[i] = interpolated_channel(0);
                        } else if host_channels == 2 && agent_channels == 1 {
                            frame[i] = (interpolated_channel(0) + interpolated_channel(1)) * 0.5;
                        } else if host_channels == 2 && agent_channels > 2 {
                            if i % 2 == 0 {
                                frame[i] = interpolated_channel(0);
                            } else {
                                frame[i] = interpolated_channel(1);
                            }
                        } else {
                            frame[i] = interpolated_channel(i);
                        }
                    }
                    
                    phase += current_factor;
                }
            },
            |err| eprintln!("an error occurred on stream: {}", err),
            None
        ) {
            Ok(s) => s,
            Err(e) => {
                logger.log(format!("Aviso: no se pudo crear el stream de salida de audio: {e}"));
                return;
            }
        };
        
        if let Err(e) = cpal_stream.play() {
            logger.log(format!("Aviso: no se pudo iniciar la reproducción de audio: {e}"));
            return;
        }
        
        let mut temp_buf = [0u8; 4096];
        let mut predictor = 0;
        let mut step_index = 0;

        let mut cipher_rx = lanpilot_core::Rc4Cipher::new(format!("{}-audio-h2c", pair_code).as_bytes());

        if use_compression {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
            
            let mut len_bytes = [0u8; 4];
            while !is_stopped(&stop) {
                if let Err(e) = std::io::Read::read_exact(&mut stream, &mut len_bytes) {
                    if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    logger.log("Desconexión en stream de audio comprimido.".to_string());
                    break;
                }
                let compressed_len = u32::from_le_bytes(len_bytes) as usize;
                let mut compressed_data = vec![0u8; compressed_len];
                if let Err(e) = std::io::Read::read_exact(&mut stream, &mut compressed_data) {
                    logger.log(format!("Error leyendo payload de audio comprimido: {e}"));
                    break;
                }
                
                cipher_rx.crypt(&mut compressed_data);
                
                if let Ok(decompressed) = decompress_size_prepended(&compressed_data) {
                    let mut buffer = audio_buffer.lock().unwrap();
                    for byte in decompressed {
                        let code1 = byte & 0x0F;
                        let code2 = (byte >> 4) & 0x0F;
                        let s1 = lanpilot_core::adpcm_decode_sample(code1, &mut predictor, &mut step_index);
                        let s2 = lanpilot_core::adpcm_decode_sample(code2, &mut predictor, &mut step_index);
                        buffer.push_back(s1);
                        buffer.push_back(s2);
                    }
                    
                    let max_latency_samples = (sample_rate as usize * channels as usize) / 4;
                    if buffer.len() > max_latency_samples {
                        let discard = buffer.len() - (max_latency_samples / 2);
                        buffer.drain(0..discard);
                        
                        let fade_len = ((sample_rate as usize * 5 / 1000) * channels as usize).min(buffer.len());
                        for i in 0..fade_len {
                            let factor = i as f32 / fade_len as f32;
                            buffer[i] = (buffer[i] as f32 * factor) as i16;
                        }
                    }
                }
            }
        } else {
            let _ = stream.set_nonblocking(true);
            while !is_stopped(&stop) {
                match std::io::Read::read(&mut stream, &mut temp_buf) {
                    Ok(0) => {
                        logger.log("Stream de audio cerrado por el host.".to_string());
                        break;
                    }
                    Ok(n) => {
                        let mut buffer = audio_buffer.lock().unwrap();
                        let bytes = &temp_buf[0..n];
                        
                        for &byte in bytes {
                            let code1 = byte & 0x0F;
                            let code2 = (byte >> 4) & 0x0F;
                            
                            let s1 = lanpilot_core::adpcm_decode_sample(code1, &mut predictor, &mut step_index);
                            let s2 = lanpilot_core::adpcm_decode_sample(code2, &mut predictor, &mut step_index);
                            
                            buffer.push_back(s1);
                            buffer.push_back(s2);
                        }
                        
                        let max_latency_samples = (sample_rate as usize * channels as usize) / 4;
                        if buffer.len() > max_latency_samples {
                            let discard = buffer.len() - (max_latency_samples / 2);
                            buffer.drain(0..discard);
                            
                            let fade_len = ((sample_rate as usize * 5 / 1000) * channels as usize).min(buffer.len());
                            for i in 0..fade_len {
                                let factor = i as f32 / fade_len as f32;
                                buffer[i] = (buffer[i] as f32 * factor) as i16;
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        logger.log(format!("Error de lectura en stream de audio: {e}"));
                        break;
                    }
                }
            }
        }
        
        let _ = cpal_stream.pause();
    });
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

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetSystemMetrics(n_index: i32) -> i32;
    fn GetKeyState(n_virt_key: i32) -> i16;
    fn SetCursorPos(x: i32, y: i32) -> i32;
}
