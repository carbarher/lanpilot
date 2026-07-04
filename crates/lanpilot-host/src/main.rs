use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lanpilot_core::{
    CONTROL_PORT, ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse,
    HANDSHAKE_PORT, HandshakeAck, HandshakeHello, PRODUCT_NAME, PROTOCOL_MAGIC, STREAM_PORT,
    StreamCompression, StreamFrame, StreamHello, TAGLINE, from_json_line, local_ipv4, to_json_line,
    unix_timestamp_ms,
};
use lz4_flex::compress_prepend_size;
use scrap::{Capturer, Display};

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

fn main() {
    let host_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "lanpilot-host".to_string());
    let host_ipv4 = local_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);

    println!("{PRODUCT_NAME} Host");
    println!("{TAGLINE}");
    println!("Listening for discovery on UDP {DISCOVERY_PORT}");
    println!("Listening for handshakes on TCP {HANDSHAKE_PORT}");
    println!("Listening for control channel on TCP {CONTROL_PORT}");
    println!("Listening for stream channel on TCP {STREAM_PORT}");
    println!("Host identity: {host_name} ({host_ipv4})");

    let discovery_name = host_name.clone();
    let discovery_ip = host_ipv4;
    let _discovery_thread =
        thread::spawn(move || run_discovery_server(&discovery_name, discovery_ip));
    let tuning = Arc::new(Mutex::new(StreamTuning::default()));
    let control_tuning = Arc::clone(&tuning);
    let stream_tuning = Arc::clone(&tuning);
    let _control_thread = thread::spawn(move || run_control_server(control_tuning));
    let _stream_thread = thread::spawn(move || run_stream_server(stream_tuning));

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

fn run_control_server(tuning: Arc<Mutex<StreamTuning>>) {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, CONTROL_PORT))
        .expect("failed to bind TCP control listener");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let tuning = Arc::clone(&tuning);
                thread::spawn(move || handle_control_stream(stream, tuning));
            }
            Err(err) => eprintln!("control incoming connection error: {err}"),
        }
    }
}

fn handle_control_stream(stream: TcpStream, tuning: Arc<Mutex<StreamTuning>>) {
    let peer = match stream.peer_addr() {
        Ok(addr) => addr.to_string(),
        Err(err) => {
            eprintln!("control peer addr error: {err}");
            "<unknown>".to_string()
        }
    };

    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let bytes_read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(err) => {
                eprintln!("control read error from {peer}: {err}");
                break;
            }
        };
        if bytes_read == 0 {
            break;
        }

        let frame: ControlFrame = match from_json_line(&line) {
            Ok(frame) => frame,
            Err(err) => {
                eprintln!("invalid control frame from {peer}: {err}");
                continue;
            }
        };

        if frame.magic != PROTOCOL_MAGIC {
            eprintln!("ignoring control frame with invalid magic from {peer}");
            continue;
        }

        println!(
            "Control frame accepted: session={} events={} source={peer}",
            frame.session_id,
            frame.events.len()
        );
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
                        eprintln!("failed to lock stream tuning");
                        continue;
                    }
                };
                let next_fps = (*target_fps).clamp(3, 30);
                let next_scale = (*scale_divisor).clamp(1, 3);
                if guard.target_fps != next_fps || guard.scale_divisor != next_scale {
                    println!(
                        "Adaptive stream update: fps {}->{} scale {}->{} (lat={}ms jitter={}ms)",
                        guard.target_fps,
                        next_fps,
                        guard.scale_divisor,
                        next_scale,
                        avg_latency_ms,
                        jitter_ms
                    );
                    guard.target_fps = next_fps;
                    guard.scale_divisor = next_scale;
                }
            }
        }
    }
}

fn run_stream_server(tuning: Arc<Mutex<StreamTuning>>) {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, STREAM_PORT))
        .expect("failed to bind TCP stream listener");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let tuning = Arc::clone(&tuning);
                thread::spawn(move || {
                    if let Err(err) = handle_stream_channel(stream, tuning) {
                        eprintln!("stream channel error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("stream incoming connection error: {err}"),
        }
    }
}

fn handle_stream_channel(
    mut stream: TcpStream,
    tuning: Arc<Mutex<StreamTuning>>,
) -> Result<(), String> {
    let peer = stream
        .peer_addr()
        .map_err(|err| format!("stream peer addr error: {err}"))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|err| format!("stream clone error: {err}"))?,
    );

    let mut hello_line = String::new();
    reader
        .read_line(&mut hello_line)
        .map_err(|err| format!("stream read hello failed: {err}"))?;
    let hello: StreamHello =
        from_json_line(&hello_line).map_err(|err| format!("invalid stream hello: {err}"))?;
    if hello.magic != PROTOCOL_MAGIC || hello.role != "agent" {
        return Err(format!("invalid stream hello payload: {:?}", hello));
    }

    println!(
        "Stream channel established: session={} agent={} source={}",
        hello.session_id, hello.agent_name, peer
    );

    let source_mode =
        std::env::var("LANPILOT_STREAM_SOURCE").unwrap_or_else(|_| "screen".to_string());
    let mut capture = if source_mode.eq_ignore_ascii_case("synthetic") {
        None
    } else {
        match ScreenCapture::new() {
            Ok(capture) => Some(capture),
            Err(err) => {
                eprintln!("stream capture unavailable, using synthetic source: {err}");
                None
            }
        }
    };

    let max_frames = std::env::var("LANPILOT_MAX_STREAM_FRAMES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(120);

    for sequence in 0..max_frames {
        let tick_start = Instant::now();
        let tuning_snapshot = match tuning.lock() {
            Ok(guard) => *guard,
            Err(_) => StreamTuning::default(),
        };
        let frame_interval_ms = (1000 / tuning_snapshot.target_fps.max(1)).max(16);
        let frame = match capture.as_mut() {
            Some(screen) => screen.capture_frame(
                hello.session_id.clone(),
                sequence,
                frame_interval_ms,
                tuning_snapshot.scale_divisor,
            )?,
            None => synthetic_frame_with_tuning(
                hello.session_id.clone(),
                sequence,
                frame_interval_ms,
                tuning_snapshot.scale_divisor,
            ),
        };
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

    Ok(())
}

struct ScreenCapture {
    capturer: Capturer,
    width: u32,
    height: u32,
}

impl ScreenCapture {
    fn new() -> Result<Self, String> {
        let display = Display::primary().map_err(|err| format!("primary display error: {err}"))?;
        let width = display.width() as u32;
        let height = display.height() as u32;
        let capturer =
            Capturer::new(display).map_err(|err| format!("capturer init error: {err}"))?;
        Ok(Self {
            capturer,
            width,
            height,
        })
    }

    fn capture_frame(
        &mut self,
        session_id: String,
        sequence: u64,
        frame_interval_ms: u32,
        scale_divisor: u8,
    ) -> Result<StreamFrame, String> {
        let mut attempts = 0;
        let bytes = loop {
            match self.capturer.frame() {
                Ok(frame) => break frame.to_vec(),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    attempts += 1;
                    if attempts > 25 {
                        return Err("screen capture timeout waiting for frame".to_string());
                    }
                    thread::sleep(Duration::from_millis(4));
                }
                Err(err) => return Err(format!("screen capture error: {err}")),
            }
        };

        let stride_bytes = bytes.len() / self.height as usize;
        let (scaled, width, height, scaled_stride) =
            normalize_and_scale_bgra(&bytes, self.width, self.height, stride_bytes, scale_divisor);
        let compressed = compress_prepend_size(&scaled);
        let encoded = BASE64.encode(compressed);
        Ok(StreamFrame {
            magic: PROTOCOL_MAGIC.to_string(),
            session_id,
            sequence,
            captured_at_ms: unix_timestamp_ms(),
            width,
            height,
            stride_bytes: scaled_stride,
            pixel_format: "bgra8".to_string(),
            compression: StreamCompression::Lz4,
            frame_interval_ms,
            compressed_payload_b64: encoded,
            raw_len: scaled.len(),
            source: "screen".to_string(),
        })
    }
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
