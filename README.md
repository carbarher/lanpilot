# LanPilot

LanPilot is a LAN-first remote desktop control project designed to pair naturally with LanCopy.

## Repository

- Name: `lanpilot`
- Suggested tagline: **LAN-first remote desktop control**

## Initial MVP structure

- `crates/lanpilot-host` - host-side desktop controller entrypoint
- `crates/lanpilot-agent` - agent-side session/runtime entrypoint
- `crates/lanpilot-core` - shared domain and protocol primitives

## Quick start

```powershell
cargo build
cargo test
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
