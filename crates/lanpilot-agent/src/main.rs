use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use lanpilot_core::{
    ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse, EdgeDirection,
    EdgeSwitchConfig, HandshakeAck, HandshakeHello, PRODUCT_NAME, PROTOCOL_MAGIC, TAGLINE,
    from_json_line, should_switch_to_remote, to_json_line,
};

fn main() -> Result<(), String> {
    let agent_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "lanpilot-agent".to_string());

    println!("{PRODUCT_NAME} Agent");
    println!("{TAGLINE}");
    println!("Sending discovery broadcast on UDP {DISCOVERY_PORT}...");

    let discovered = discover_host(&agent_name)?;
    println!(
        "Discovered host {} at {}:{}",
        discovered.host_name, discovered.host_ipv4, discovered.handshake_port
    );

    let ack = perform_handshake(&agent_name, &discovered)?;
    println!(
        "Handshake OK with host={} session={}",
        ack.host_name, ack.session_id
    );

    run_phase2_input_channel(&discovered.host_ipv4, &ack)?;

    Ok(())
}

fn discover_host(agent_name: &str) -> Result<DiscoveryResponse, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|err| format!("bind discovery socket failed: {err}"))?;
    socket
        .set_broadcast(true)
        .map_err(|err| format!("set broadcast failed: {err}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(2_000)))
        .map_err(|err| format!("set read timeout failed: {err}"))?;

    let probe = DiscoveryProbe::new(agent_name.to_string());
    let payload = to_json_line(&probe).map_err(|err| format!("encode probe failed: {err}"))?;
    let target = SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT));
    socket
        .send_to(payload.as_bytes(), target)
        .map_err(|err| format!("send discovery failed: {err}"))?;

    let mut buf = [0_u8; 2048];
    let (received, source) = socket
        .recv_from(&mut buf)
        .map_err(|err| format!("receive discovery response failed: {err}"))?;
    let response_line = std::str::from_utf8(&buf[..received])
        .map_err(|err| format!("utf8 discovery response failed: {err}"))?;
    let response: DiscoveryResponse = from_json_line(response_line)
        .map_err(|err| format!("decode discovery response failed: {err}"))?;

    if response.magic != PROTOCOL_MAGIC {
        return Err(format!(
            "invalid discovery response magic from {source}: {}",
            response.magic
        ));
    }
    Ok(response)
}

fn perform_handshake(
    agent_name: &str,
    discovered: &DiscoveryResponse,
) -> Result<HandshakeAck, String> {
    let endpoint = format!("{}:{}", discovered.host_ipv4, discovered.handshake_port);
    let mut stream = TcpStream::connect(endpoint.as_str())
        .map_err(|err| format!("connect handshake socket failed: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|err| format!("set read timeout failed: {err}"))?;

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

fn run_phase2_input_channel(host_ipv4: &str, ack: &HandshakeAck) -> Result<(), String> {
    let config = EdgeSwitchConfig::right_default(1920);
    let cursor_x = config.screen_width_px - 1;
    let cursor_y = 540;
    if !should_switch_to_remote(cursor_x, &config) {
        println!("Edge switch not triggered; remote input channel idle.");
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
    println!(
        "Phase 2: edge-switch triggered, sent {} input events to control channel {}",
        frame.events.len(),
        endpoint
    );
    Ok(())
}
