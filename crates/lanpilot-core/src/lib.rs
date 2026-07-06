use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};

// ── Logger ───────────────────────────────────────────────────────────────────

/// A cheaply-clonable status callback used by the host/agent runtime
/// libraries to stream human-readable status lines to whatever is driving
/// them: a CLI binary (stdout), the GUI app (an in-memory log), or a test
/// harness (a `Vec<String>` collector).
#[derive(Clone)]
pub struct Logger(Arc<dyn Fn(String) + Send + Sync>);

impl Logger {
    /// Build a logger from any `Fn(String)` callback.
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        Logger(Arc::new(callback))
    }

    /// Emit a status line. Accepts `String` or `&str` via `Into<String>`.
    pub fn log(&self, message: impl Into<String>) {
        (self.0)(message.into());
    }

    /// Logger that writes every line to stdout, used by the CLI binaries.
    pub fn stdout() -> Self {
        Logger::new(|message| println!("{message}"))
    }
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Logger(..)")
    }
}

// ── StopFlag ─────────────────────────────────────────────────────────────────

/// Shared cooperative-cancellation flag threaded through host/agent runtime
/// loops so callers (e.g. a GUI Stop/Cancel button) can ask a background
/// thread to wind down without killing the whole process.
pub type StopFlag = Arc<AtomicBool>;

/// Build a fresh, unset stop flag.
pub fn new_stop_flag() -> StopFlag {
    Arc::new(AtomicBool::new(false))
}

/// Returns `true` once someone has requested cancellation via the flag.
pub fn is_stopped(flag: &StopFlag) -> bool {
    flag.load(Ordering::Relaxed)
}

pub const PRODUCT_NAME: &str = "LanPilot";
pub const TAGLINE: &str = "LAN-first remote desktop control";
pub const PROTOCOL_MAGIC: &str = "LANPILOT_V1";
pub const DISCOVERY_PORT: u16 = 47042;
pub const HANDSHAKE_PORT: u16 = 47043;
pub const CONTROL_PORT: u16 = 47044;
pub const STREAM_PORT: u16 = 47045;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub machine_name: String,
    pub ipv4: String,
}

impl NodeIdentity {
    pub fn new(machine_name: impl Into<String>, ipv4: impl Into<String>) -> Self {
        Self {
            machine_name: machine_name.into(),
            ipv4: ipv4.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryProbe {
    pub magic: String,
    pub agent_name: String,
    pub pair_code: String,
}

impl DiscoveryProbe {
    pub fn new(agent_name: impl Into<String>, pair_code: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            agent_name: agent_name.into(),
            pair_code: pair_code.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryResponse {
    pub magic: String,
    pub host_name: String,
    pub host_ipv4: String,
    pub handshake_port: u16,
}

impl DiscoveryResponse {
    pub fn new(host_name: impl Into<String>, host_ipv4: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            host_name: host_name.into(),
            host_ipv4: host_ipv4.into(),
            handshake_port: HANDSHAKE_PORT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeHello {
    pub magic: String,
    pub role: String,
    pub agent_name: String,
}

impl HandshakeHello {
    pub fn new(agent_name: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            role: "agent".to_string(),
            agent_name: agent_name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeAck {
    pub magic: String,
    pub status: String,
    pub host_name: String,
    pub session_id: String,
    pub control_port: u16,
    pub stream_port: u16,
}

impl HandshakeAck {
    pub fn ok(host_name: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            status: "ok".to_string(),
            host_name: host_name.into(),
            session_id: generate_session_id(),
            control_port: CONTROL_PORT,
            stream_port: STREAM_PORT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDirection {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSwitchConfig {
    pub edge: EdgeDirection,
    pub threshold_px: i32,
    pub screen_width_px: i32,
}

impl EdgeSwitchConfig {
    pub fn right_default(screen_width_px: i32) -> Self {
        Self {
            edge: EdgeDirection::Right,
            threshold_px: 4,
            screen_width_px,
        }
    }
}

pub fn should_switch_to_remote(cursor_x: i32, config: &EdgeSwitchConfig) -> bool {
    match config.edge {
        EdgeDirection::Left => cursor_x <= config.threshold_px,
        EdgeDirection::Right => cursor_x >= config.screen_width_px - config.threshold_px,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    EdgeSwitch {
        edge: EdgeDirection,
        cursor_x: i32,
        cursor_y: i32,
    },
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: String,
        pressed: bool,
    },
    Key {
        key: String,
        pressed: bool,
    },
    StreamFeedback {
        target_fps: u32,
        scale_divisor: u8,
        avg_latency_ms: u32,
        jitter_ms: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFrame {
    pub magic: String,
    pub session_id: String,
    pub events: Vec<ControlEvent>,
}

impl ControlFrame {
    pub fn new(session_id: impl Into<String>, events: Vec<ControlEvent>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            session_id: session_id.into(),
            events,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHello {
    pub magic: String,
    pub role: String,
    pub session_id: String,
    pub agent_name: String,
}

impl StreamHello {
    pub fn new(session_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            role: "agent".to_string(),
            session_id: session_id.into(),
            agent_name: agent_name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamFrame {
    pub magic: String,
    pub session_id: String,
    pub sequence: u64,
    pub captured_at_ms: u128,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: usize,
    pub pixel_format: String,
    pub compression: StreamCompression,
    pub frame_interval_ms: u32,
    pub compressed_payload_b64: String,
    pub raw_len: usize,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamCompression {
    None,
    Lz4,
}

impl StreamFrame {
    pub fn synthetic(session_id: impl Into<String>, sequence: u64) -> Self {
        let width = 960;
        let height = 540;
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            session_id: session_id.into(),
            sequence,
            captured_at_ms: unix_timestamp_ms(),
            width,
            height,
            stride_bytes: width as usize * 4,
            pixel_format: "rgba8".to_string(),
            compression: StreamCompression::None,
            frame_interval_ms: 100,
            compressed_payload_b64: String::new(),
            raw_len: width as usize * height as usize * 4,
            source: "synthetic".to_string(),
        }
    }
}

pub fn to_json_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

pub fn from_json_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

pub fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let local = socket.local_addr().ok()?;
    match local.ip() {
        std::net::IpAddr::V4(ipv4) => Some(ipv4),
        std::net::IpAddr::V6(_) => None,
    }
}

pub fn normalize_pair_code(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 6 {
        Some(digits)
    } else {
        None
    }
}

pub fn generate_pair_code() -> String {
    let code = rand::thread_rng().gen_range(0u32..1_000_000);
    format!("{code:06}")
}

fn generate_session_id() -> String {
    let id: u64 = rand::thread_rng().r#gen();
    format!("lp-{id:016x}")
}

pub fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

// ── IpRateLimiter ────────────────────────────────────────────────────────────

/// Fixed-window rate limiter keyed by source IP.
pub struct IpRateLimiter {
    counts: HashMap<IpAddr, (u32, Instant)>,
    max_count: u32,
    window: Duration,
}

impl IpRateLimiter {
    pub fn new(max_count: u32, window: Duration) -> Self {
        Self {
            counts: HashMap::new(),
            max_count,
            window,
        }
    }

    /// Returns `true` if the request is within the rate limit, `false` if it
    /// should be dropped.  Resets the window counter once `window` has elapsed.
    pub fn check_and_record(&mut self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let entry = self.counts.entry(ip).or_insert((0, now));
        if now.duration_since(entry.1) >= self.window {
            *entry = (0, now);
        }
        if entry.0 >= self.max_count {
            return false;
        }
        entry.0 += 1;
        true
    }
}

// ── SessionEvent ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Created {
        session_id: String,
        peer_ip: String,
        agent_name: String,
        timestamp_ms: u128,
    },
    Refreshed {
        session_id: String,
        timestamp_ms: u128,
    },
    Expired {
        session_id: String,
        reason: String,
        timestamp_ms: u128,
    },
    Removed {
        session_id: String,
        timestamp_ms: u128,
    },
    ConnectionDropped {
        peer_ip: String,
        reason: String,
        timestamp_ms: u128,
    },
    RateLimitDrop {
        peer_ip: String,
        endpoint: String,
        timestamp_ms: u128,
    },
}

pub fn session_event_json(event: &SessionEvent) -> String {
    match event {
        SessionEvent::Created { session_id, peer_ip, agent_name, timestamp_ms } => serde_json::json!({
            "event": "session_created",
            "session_id": session_id,
            "peer_ip": peer_ip,
            "agent_name": agent_name,
            "timestamp_ms": timestamp_ms,
        })
        .to_string(),
        SessionEvent::Refreshed { session_id, timestamp_ms } => serde_json::json!({
            "event": "session_refreshed",
            "session_id": session_id,
            "timestamp_ms": timestamp_ms,
        })
        .to_string(),
        SessionEvent::Expired { session_id, reason, timestamp_ms } => serde_json::json!({
            "event": "session_expired",
            "session_id": session_id,
            "reason": reason,
            "timestamp_ms": timestamp_ms,
        })
        .to_string(),
        SessionEvent::Removed { session_id, timestamp_ms } => serde_json::json!({
            "event": "session_removed",
            "session_id": session_id,
            "timestamp_ms": timestamp_ms,
        })
        .to_string(),
        SessionEvent::ConnectionDropped { peer_ip, reason, timestamp_ms } => serde_json::json!({
            "event": "connection_dropped",
            "peer_ip": peer_ip,
            "reason": reason,
            "timestamp_ms": timestamp_ms,
        })
        .to_string(),
        SessionEvent::RateLimitDrop { peer_ip, endpoint, timestamp_ms } => serde_json::json!({
            "event": "rate_limit_drop",
            "peer_ip": peer_ip,
            "endpoint": endpoint,
            "timestamp_ms": timestamp_ms,
        })
        .to_string(),
    }
}

/// Prints a structured JSON telemetry line to stdout.
pub fn log_session_event(event: &SessionEvent) {
    println!("{}", session_event_json(event));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_constants_are_stable() {
        assert_eq!(PRODUCT_NAME, "LanPilot");
        assert_eq!(TAGLINE, "LAN-first remote desktop control");
    }

    #[test]
    fn node_identity_builder_sets_fields() {
        let node = NodeIdentity::new("pc-host", "192.168.1.33");
        assert_eq!(node.machine_name, "pc-host");
        assert_eq!(node.ipv4, "192.168.1.33");
    }

    #[test]
    fn discovery_probe_roundtrip_json_line() {
        let probe = DiscoveryProbe::new("pc-agent", "123456");
        let line = to_json_line(&probe).expect("must serialize probe");
        let decoded: DiscoveryProbe = from_json_line(&line).expect("must deserialize probe");
        assert_eq!(decoded, probe);
    }

    #[test]
    fn normalize_pair_code_accepts_six_digits() {
        assert_eq!(normalize_pair_code("12 34-56"), Some("123456".to_string()));
        assert_eq!(normalize_pair_code("12345"), None);
        assert_eq!(normalize_pair_code("1234567"), None);
    }

    #[test]
    fn invalid_json_line_fails() {
        let decoded = from_json_line::<DiscoveryProbe>("not-json");
        assert!(decoded.is_err());
    }

    #[test]
    fn handshake_ack_contains_prefix() {
        let ack = HandshakeAck::ok("pc-host");
        assert_eq!(ack.status, "ok");
        assert_eq!(ack.control_port, CONTROL_PORT);
        assert_eq!(ack.stream_port, STREAM_PORT);
        assert!(ack.session_id.starts_with("lp-"));
    }

    #[test]
    fn should_switch_right_edge_when_threshold_reached() {
        let config = EdgeSwitchConfig::right_default(1920);
        assert!(!should_switch_to_remote(1800, &config));
        assert!(should_switch_to_remote(1918, &config));
    }

    #[test]
    fn control_frame_roundtrip_json_line() {
        let frame = ControlFrame::new(
            "lp-1",
            vec![
                ControlEvent::EdgeSwitch {
                    edge: EdgeDirection::Right,
                    cursor_x: 1919,
                    cursor_y: 540,
                },
                ControlEvent::MouseMove { dx: 12, dy: -4 },
                ControlEvent::MouseButton {
                    button: "left".to_string(),
                    pressed: true,
                },
                ControlEvent::StreamFeedback {
                    target_fps: 8,
                    scale_divisor: 2,
                    avg_latency_ms: 190,
                    jitter_ms: 85,
                },
            ],
        );
        let line = to_json_line(&frame).expect("must serialize control frame");
        let decoded: ControlFrame = from_json_line(&line).expect("must deserialize control frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn session_event_json_escapes_quote_characters() {
        let event = SessionEvent::Expired {
            session_id: "lp-test".to_string(),
            reason: "rdp \"black frame\" fallback".to_string(),
            timestamp_ms: 42,
        };
        let json = session_event_json(&event);
        let value: serde_json::Value = serde_json::from_str(&json).expect("must be valid json");
        assert_eq!(value["event"], "session_expired");
        assert_eq!(value["reason"], "rdp \"black frame\" fallback");
    }

    #[test]
    fn stream_frame_roundtrip_json_line() {
        let frame = StreamFrame::synthetic("lp-xyz", 7);
        let line = to_json_line(&frame).expect("must serialize stream frame");
        let decoded: StreamFrame = from_json_line(&line).expect("must deserialize stream frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn stream_frame_synthetic_fields_are_stable() {
        let frame = StreamFrame::synthetic("lp-xyz", 1);
        assert_eq!(frame.compression, StreamCompression::None);
        assert_eq!(frame.source, "synthetic");
        assert_eq!(frame.frame_interval_ms, 100);
        assert_eq!(frame.stride_bytes, frame.width as usize * 4);
        assert!(frame.compressed_payload_b64.is_empty());
    }

    // ── IpRateLimiter tests ───────────────────────────────────────────────────

    use std::net::{IpAddr, Ipv4Addr as TestIp4};
    use std::time::Duration;

    #[test]
    fn rate_limiter_within_limit_is_allowed() {
        let mut limiter = IpRateLimiter::new(5, Duration::from_secs(10));
        let ip = IpAddr::V4(TestIp4::new(127, 0, 0, 1));
        for _ in 0..5 {
            assert!(limiter.check_and_record(ip), "each request within limit must be allowed");
        }
    }

    #[test]
    fn rate_limiter_over_limit_is_denied() {
        let mut limiter = IpRateLimiter::new(5, Duration::from_secs(10));
        let ip = IpAddr::V4(TestIp4::new(127, 0, 0, 1));
        for _ in 0..5 {
            limiter.check_and_record(ip);
        }
        assert!(!limiter.check_and_record(ip), "6th request must be denied");
    }

    #[test]
    fn rate_limiter_window_expiry_resets_count() {
        let mut limiter = IpRateLimiter::new(2, Duration::from_millis(20));
        let ip = IpAddr::V4(TestIp4::new(127, 0, 0, 1));
        assert!(limiter.check_and_record(ip));
        assert!(limiter.check_and_record(ip));
        assert!(!limiter.check_and_record(ip), "3rd in same window must be denied");
        std::thread::sleep(Duration::from_millis(30));
        assert!(limiter.check_and_record(ip), "after window expiry must be allowed again");
    }

    #[test]
    fn session_events_log_without_panicking() {
        let events = [
            SessionEvent::Created {
                session_id: "lp-a".to_string(),
                peer_ip: "192.168.1.2".to_string(),
                agent_name: "agent".to_string(),
                timestamp_ms: 1,
            },
            SessionEvent::Refreshed {
                session_id: "lp-a".to_string(),
                timestamp_ms: 2,
            },
            SessionEvent::Expired {
                session_id: "lp-a".to_string(),
                reason: "ttl".to_string(),
                timestamp_ms: 3,
            },
            SessionEvent::Removed {
                session_id: "lp-a".to_string(),
                timestamp_ms: 4,
            },
            SessionEvent::ConnectionDropped {
                peer_ip: "192.168.1.2".to_string(),
                reason: "reset".to_string(),
                timestamp_ms: 5,
            },
            SessionEvent::RateLimitDrop {
                peer_ip: "192.168.1.2".to_string(),
                endpoint: "discovery".to_string(),
                timestamp_ms: 6,
            },
        ];

        for event in &events {
            log_session_event(event);
        }
    }

    // ── SessionEvent tests ────────────────────────────────────────────────────

    #[test]
    fn log_session_event_created_does_not_panic() {
        log_session_event(&SessionEvent::Created {
            session_id: "lp-abc123".to_string(),
            peer_ip: "192.168.1.5".to_string(),
            agent_name: "pc-agent".to_string(),
            timestamp_ms: 1_234_567_890,
        });
    }

    #[test]
    fn log_session_event_connection_dropped_does_not_panic() {
        log_session_event(&SessionEvent::ConnectionDropped {
            peer_ip: "192.168.1.5".to_string(),
            reason: "broken pipe".to_string(),
            timestamp_ms: 1_234_567_890,
        });
    }

    #[test]
    fn log_session_event_rate_limit_drop_does_not_panic() {
        log_session_event(&SessionEvent::RateLimitDrop {
            peer_ip: "10.0.0.1".to_string(),
            endpoint: "discovery".to_string(),
            timestamp_ms: 1_234_567_890,
        });
    }

    // ── Logger / StopFlag tests ───────────────────────────────────────────────

    #[test]
    fn logger_forwards_messages_to_callback() {
        let received: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let logger = Logger::new(move |line| sink.lock().unwrap().push(line));
        logger.log("hello");
        logger.log(String::from("world"));
        assert_eq!(*received.lock().unwrap(), vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn logger_is_cheaply_cloneable_and_shares_state() {
        let received: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let logger = Logger::new(move |line| sink.lock().unwrap().push(line));
        let cloned = logger.clone();
        cloned.log("from-clone");
        assert_eq!(*received.lock().unwrap(), vec!["from-clone".to_string()]);
    }

    #[test]
    fn stop_flag_starts_unset_and_can_be_flipped() {
        let flag = new_stop_flag();
        assert!(!is_stopped(&flag));
        flag.store(true, Ordering::Relaxed);
        assert!(is_stopped(&flag));
    }
}
