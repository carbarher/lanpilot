use std::net::{Ipv4Addr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
}

impl DiscoveryProbe {
    pub fn new(agent_name: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            agent_name: agent_name.into(),
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
        let payload = format!("synthetic-frame-{sequence}");
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            session_id: session_id.into(),
            sequence,
            captured_at_ms: unix_timestamp_ms(),
            width: 1280,
            height: 720,
            pixel_format: "rgba8".to_string(),
            compression: StreamCompression::None,
            frame_interval_ms: 100,
            compressed_payload_b64: payload,
            raw_len: 0,
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

fn generate_session_id() -> String {
    let millis = unix_timestamp_ms();
    format!("lp-{millis}")
}

pub fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
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
        let probe = DiscoveryProbe::new("pc-agent");
        let line = to_json_line(&probe).expect("must serialize probe");
        let decoded: DiscoveryProbe = from_json_line(&line).expect("must deserialize probe");
        assert_eq!(decoded, probe);
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
            ],
        );
        let line = to_json_line(&frame).expect("must serialize control frame");
        let decoded: ControlFrame = from_json_line(&line).expect("must deserialize control frame");
        assert_eq!(decoded, frame);
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
        assert!(frame.compressed_payload_b64.starts_with("synthetic-frame-"));
    }
}
