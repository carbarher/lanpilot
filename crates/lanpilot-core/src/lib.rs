use std::net::{Ipv4Addr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const PRODUCT_NAME: &str = "LanPilot";
pub const TAGLINE: &str = "LAN-first remote desktop control";
pub const PROTOCOL_MAGIC: &str = "LANPILOT_V1";
pub const DISCOVERY_PORT: u16 = 47042;
pub const HANDSHAKE_PORT: u16 = 47043;

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
}

impl HandshakeAck {
    pub fn ok(host_name: impl Into<String>) -> Self {
        Self {
            magic: PROTOCOL_MAGIC.to_string(),
            status: "ok".to_string(),
            host_name: host_name.into(),
            session_id: generate_session_id(),
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
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("lp-{millis}")
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
        assert!(ack.session_id.starts_with("lp-"));
    }
}
