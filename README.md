# LanPilot

LanPilot is a LAN-first remote desktop control project designed to pair naturally with LanCopy.

## Repository

- Name: `lanpilot`
- Suggested tagline: **LAN-first remote desktop control**

## Initial MVP structure

- `src/LanPilot.Host` - host-side desktop controller entrypoint
- `src/LanPilot.Agent` - agent-side session/runtime entrypoint
- `src/LanPilot.Core` - shared domain and protocol primitives
- `tests/LanPilot.Core.Tests` - unit tests for shared core

## Quick start

```powershell
dotnet build .\LanPilot.slnx
dotnet test .\LanPilot.slnx
dotnet run --project .\src\LanPilot.Host\LanPilot.Host.csproj
dotnet run --project .\src\LanPilot.Agent\LanPilot.Agent.csproj
```
