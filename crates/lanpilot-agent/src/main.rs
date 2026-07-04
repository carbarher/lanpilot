use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lanpilot_core::{
    ControlEvent, ControlFrame, DISCOVERY_PORT, DiscoveryProbe, DiscoveryResponse, EdgeDirection,
    EdgeSwitchConfig, HandshakeAck, HandshakeHello, PRODUCT_NAME, PROTOCOL_MAGIC,
    StreamCompression, StreamFrame, StreamHello, TAGLINE, from_json_line, normalize_pair_code,
    should_switch_to_remote, to_json_line, unix_timestamp_ms,
};
use lz4_flex::decompress_size_prepended;
use minifb::{Scale, Window, WindowOptions};

fn main() -> Result<(), String> {
    let agent_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "lanpilot-agent".to_string());

    println!("{PRODUCT_NAME} Agent");
    println!("{TAGLINE}");
    println!();
    println!("=== Conectarme ===");
    let pair_code = read_pair_code()?;
    println!("Sending discovery broadcast on UDP {DISCOVERY_PORT}...");

    let discovered = discover_host(&agent_name, &pair_code)?;
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
    run_phase3_stream_channel(&discovered.host_ipv4, &agent_name, &ack)?;

    Ok(())
}

fn discover_host(agent_name: &str, pair_code: &str) -> Result<DiscoveryResponse, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|err| format!("bind discovery socket failed: {err}"))?;
    socket
        .set_broadcast(true)
        .map_err(|err| format!("set broadcast failed: {err}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(2_000)))
        .map_err(|err| format!("set read timeout failed: {err}"))?;

    let probe = DiscoveryProbe::new(agent_name.to_string(), pair_code.to_string());
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

fn read_pair_code() -> Result<String, String> {
    if let Ok(raw) = std::env::var("LANPILOT_PAIR_CODE") {
        if let Some(code) = normalize_pair_code(&raw) {
            println!("Usando codigo de conexion desde LANPILOT_PAIR_CODE.");
            return Ok(code);
        }
        return Err("LANPILOT_PAIR_CODE debe tener exactamente 6 digitos".to_string());
    }

    print!("Introduce el codigo de 6 digitos: ");
    io::stdout()
        .flush()
        .map_err(|err| format!("flush stdout failed: {err}"))?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|err| format!("read pair code failed: {err}"))?;
    normalize_pair_code(&input).ok_or_else(|| "Codigo invalido: usa 6 digitos".to_string())
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

fn run_phase3_stream_channel(
    host_ipv4: &str,
    agent_name: &str,
    ack: &HandshakeAck,
) -> Result<(), String> {
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
    let mut renderer = FrameRenderer::try_new()?;
    let mut metrics = StreamMetrics::new();
    let target_frames = std::env::var("LANPILOT_STREAM_FRAMES")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(60);

    let mut received = 0_u32;
    let mut last_sequence = 0_u64;
    let mut total_raw = 0_usize;
    let mut last_feedback: Option<(u32, u8)> = None;

    while received < target_frames {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|err| format!("read stream frame failed: {err}"))?;
        if bytes_read == 0 {
            break;
        }
        let frame: StreamFrame =
            from_json_line(&line).map_err(|err| format!("decode stream frame failed: {err}"))?;
        if frame.magic != PROTOCOL_MAGIC || frame.session_id != ack.session_id {
            return Err(format!("invalid stream frame payload: {:?}", frame));
        }

        let raw = decode_stream_frame(&frame)?;
        if let Some(renderer) = renderer.as_mut() {
            renderer.render(&frame, &raw)?;
            if !renderer.is_open() {
                break;
            }
        }

        received += 1;
        last_sequence = frame.sequence;
        total_raw += raw.len();
        metrics.observe(frame.captured_at_ms);

        if received % 10 == 0 {
            let summary = metrics.summary();
            let (target_fps, scale_divisor) = choose_adaptive_target(&summary);
            let next_feedback = (target_fps, scale_divisor);
            if last_feedback != Some(next_feedback) {
                send_stream_feedback(host_ipv4, ack, &summary, target_fps, scale_divisor)?;
                last_feedback = Some(next_feedback);
            }
        }
    }

    let summary = metrics.summary();
    println!(
        "Phase 5: stream/render active, frames={} last_seq={} raw={} fps={:.2} avg_latency_ms={:.2} jitter_ms={:.2}",
        received, last_sequence, total_raw, summary.fps, summary.avg_latency_ms, summary.jitter_ms
    );
    Ok(())
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
    host_ipv4: &str,
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
    let endpoint = format!("{}:{}", host_ipv4, ack.control_port);
    let mut stream = TcpStream::connect(endpoint.as_str())
        .map_err(|err| format!("connect feedback socket failed: {err}"))?;
    let encoded =
        to_json_line(&feedback).map_err(|err| format!("encode feedback frame failed: {err}"))?;
    stream
        .write_all(encoded.as_bytes())
        .map_err(|err| format!("send feedback frame failed: {err}"))?;
    Ok(())
}

fn decode_stream_frame(frame: &StreamFrame) -> Result<Vec<u8>, String> {
    match frame.compression {
        StreamCompression::None => Ok(generate_synthetic_bgra(frame)),
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
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let blue = ((x + frame.sequence as usize) % 256) as u8;
            let green = ((y + frame.sequence as usize * 2) % 256) as u8;
            let red = ((x / 2 + y / 2 + frame.sequence as usize) % 256) as u8;
            raw[idx] = blue;
            raw[idx + 1] = green;
            raw[idx + 2] = red;
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
}

impl FrameRenderer {
    fn try_new() -> Result<Option<Self>, String> {
        let render_enabled = std::env::var("LANPILOT_RENDER")
            .map(|raw| raw != "0")
            .unwrap_or(true);
        if !render_enabled {
            return Ok(None);
        }

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
        Ok(Some(Self {
            window,
            width,
            height,
            buffer: vec![0_u32; width * height],
        }))
    }

    fn render(&mut self, frame: &StreamFrame, raw: &[u8]) -> Result<(), String> {
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

    fn observe(&mut self, captured_at_ms: u128) {
        let now = Instant::now();
        self.frame_count += 1;

        let now_ms = unix_timestamp_ms() as f64;
        let captured_ms = captured_at_ms as f64;
        if now_ms >= captured_ms {
            self.total_latency_ms += now_ms - captured_ms;
        }

        if let Some(previous) = self.previous_arrival {
            let interval_ms = (now - previous).as_secs_f64() * 1000.0;
            let expected_ms = 100.0;
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
