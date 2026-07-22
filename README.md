@'
# LumaForge CDP Proxy

Windows proxy DLL for injecting a Chrome DevTools Protocol debugging port into Steam WebHelper processes.

## Current status

Early development and testing.

## Behavior

- Hooks `CreateProcessW`.
- Detects Steam WebHelper process creation.
- Injects `--remote-debugging-port=<port>`.
- Supports `STEAMCDP_PORT`.
- Selects a dynamic local port when no port is configured.
- Falls back to port `9222`.
- Publishes CDP discovery information for LumaForge.

## Build

```powershell
cargo fmt --check
cargo check
cargo test
cargo build --release