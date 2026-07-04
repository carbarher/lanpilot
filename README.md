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
cargo run -p lanpilot-host
cargo run -p lanpilot-agent
```
