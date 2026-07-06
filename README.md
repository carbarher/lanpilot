# LanPilot

LanPilot is a LAN-first remote desktop control project designed to pair naturally with LanCopy.

## Repository

- Name: `lanpilot`
- Suggested tagline: **LAN-first remote desktop control**

## Initial MVP structure

- `crates/lanpilot-host` - host-side desktop controller entrypoint
- `crates/lanpilot-agent` - agent-side session/runtime entrypoint
- `crates/lanpilot-core` - shared domain and protocol primitives
- `crates/lanpilot-app` - simple Windows GUI wrapper for normal users

## Quick start

```powershell
cargo build
cargo test
```

### Simple flow (for normal users)

#### Simple GUI app

Build everything:

```powershell
cargo build
```

Then run the simple app:

```powershell
cargo run -p lanpilot-app
```

What normal users see:

- **Compartir mi pantalla** → leaves this PC waiting for incoming connection
- **Buscar equipos** → lists available PCs by name in the local network
- **Conexión rápida (1 clic)** → finds and connects automatically to the best available PC
- **Conectarme** → connects this PC to the selected PC name
- **Reconectar al último equipo** → quick reconnect to the last successful target
  (if that PC changed IP, LanPilot tries to recover by PC name)
- If multiple candidates exist, LanPilot rotates automatically until one responds
- Candidate rotation uses a fast parallel probe to connect quicker on crowded LANs
- During connection, LanPilot shows live metrics (discovery/probe/handshake/retry timings)
- **Diagnóstico** → checks LAN discovery + session hints when connection fails
- live status text from the in-process background worker

`lanpilot-app` is now a true single executable: it does not need `lanpilot-host.exe`
or `lanpilot-agent.exe` beside it.

#### Portable package

Build a portable package for normal users:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-portable.ps1
```

This creates:

- `dist\LanPilot-Portable\LanPilot.exe`
- `dist\LanPilot-Portable\LEEME.txt`
- `dist\LanPilot-Portable.zip`
- `dist\LanPilot-Portable-checksums.txt`
- `dist\LanPilot-ReleaseInfo.txt`
- `dist\LanPilot-Changes.txt`
- `%USERPROFILE%\Desktop\LanPilot.exe`

Normal users only need to unzip the package and run `LanPilot.exe`.

RDP note:

- LanPilot works better when the remote RDP session is not minimized.
- Keep the remote session unlocked while sharing screen.

1. On host PC:

```powershell
cargo run -p lanpilot-host
```

You will see a 6-digit connection code.

2. On client PC:

```powershell
cargo run -p lanpilot-agent
```

Enter the same 6-digit code when asked.

Optional non-interactive mode:

```powershell
$env:LANPILOT_PAIR_CODE = "123456"
cargo run -p lanpilot-host
cargo run -p lanpilot-agent
```

### Phase 1 MVP: discovery + handshake

Start host (terminal 1):

```powershell
cargo run -p lanpilot-host
```

Run agent (terminal 2, same LAN):

```powershell
cargo run -p lanpilot-agent
```

Expected behavior:

- agent broadcasts discovery over UDP (`47042`)
- host responds with identity + handshake endpoint
- agent opens TCP handshake (`47043`) and receives session ack

### Phase 2 MVP: edge switch + remote input channel

After the handshake succeeds:

- agent evaluates right-edge crossing (`EdgeSwitchConfig`)
- when edge threshold is reached, agent sends a control frame over TCP (`47044`)
- host accepts the control frame and logs the remote input event batch

### Phase 3 MVP: initial screen stream transport

After Phase 2:

- agent opens stream channel on TCP (`47045`)
- agent sends `StreamHello` with current session id
- host streams synthetic frames (`StreamFrame`) to validate capture/transport flow

### Phase 4 MVP: real capture + compression + frame timing

- host captures real desktop frames with `scrap` (default mode)
- host compresses frames with LZ4 and sends base64 payloads in `StreamFrame`
- host applies frame pacing (`frame_interval_ms`, target ~10 FPS)
- agent validates and decompresses incoming frames

### Phase 5 MVP: agent rendering + stream metrics

- agent renders incoming stream frames in a window (`minifb`)
- stream metrics are printed: FPS, average latency, jitter
- renderer can be disabled for headless validation:

```powershell
$env:LANPILOT_RENDER = "0"
cargo run -p lanpilot-agent
```

### Phase 6 MVP: adaptive bitrate/FPS

- agent sends runtime `StreamFeedback` over control channel
- host adapts stream settings in-flight:
  - `target_fps` (frame pacing)
  - `scale_divisor` (effective stream bitrate/size)
- adaptation decisions are based on observed latency/jitter windows

Optional fallback for environments without desktop capture:

```powershell
$env:LANPILOT_STREAM_SOURCE = "synthetic"
cargo run -p lanpilot-host
```
