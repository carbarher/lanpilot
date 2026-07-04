use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::thread;

use lanpilot_core::{
    DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse, HANDSHAKE_PORT, HandshakeAck,
    HandshakeHello, PRODUCT_NAME, PROTOCOL_MAGIC, TAGLINE, from_json_line, local_ipv4,
    to_json_line,
};

fn main() {
    let host_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "lanpilot-host".to_string());
    let host_ipv4 = local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);

    println!("{PRODUCT_NAME} Host");
    println!("{TAGLINE}");
    println!("Listening for discovery on UDP {DISCOVERY_PORT}");
    println!("Listening for handshakes on TCP {HANDSHAKE_PORT}");
    println!("Host identity: {host_name} ({host_ipv4})");

    let discovery_name = host_name.clone();
    let discovery_ip = host_ipv4;
    let _discovery_thread =
        thread::spawn(move || run_discovery_server(&discovery_name, discovery_ip));

    run_handshake_server(&host_name);
}

fn run_discovery_server(host_name: &str, host_ipv4: Ipv4Addr) {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))
        .expect("failed to bind UDP discovery socket");
    let mut buffer = [0_u8; 2048];

    loop {
        let (received, source) = match socket.recv_from(&mut buffer) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("discovery receive error: {err}");
                continue;
            }
        };

        let payload = match std::str::from_utf8(&buffer[..received]) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("discovery utf8 error from {source}: {err}");
                continue;
            }
        };

        let probe: DiscoveryProbe = match from_json_line(payload) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("invalid discovery probe from {source}: {err}");
                continue;
            }
        };

        if probe.magic != PROTOCOL_MAGIC {
            eprintln!("ignoring probe with invalid magic from {source}");
            continue;
        }

        let response = DiscoveryResponse::new(host_name, host_ipv4.to_string());
        let line = match to_json_line(&response) {
            Ok(line) => line,
            Err(err) => {
                eprintln!("failed to serialize discovery response: {err}");
                continue;
            }
        };

        if let Err(err) = socket.send_to(line.as_bytes(), source) {
            eprintln!("failed sending discovery response to {source}: {err}");
        }
    }
}

fn run_handshake_server(host_name: &str) {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, HANDSHAKE_PORT))
        .expect("failed to bind TCP handshake listener");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let host_name = host_name.to_string();
                thread::spawn(move || {
                    if let Err(err) = handle_handshake(stream, &host_name) {
                        eprintln!("handshake error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("incoming connection error: {err}"),
        }
    }
}

fn handle_handshake(mut stream: TcpStream, host_name: &str) -> Result<(), String> {
    let remote: SocketAddr = stream
        .peer_addr()
        .map_err(|err| format!("peer addr error: {err}"))?;

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|err| format!("clone stream error: {err}"))?,
    );

    let mut line = String::new();
    reader
        .read_line(&mut line)
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
    println!(
        "Handshake accepted: agent={} remote={} session={}",
        hello.agent_name, source_ip, ack.session_id
    );
    Ok(())
}
