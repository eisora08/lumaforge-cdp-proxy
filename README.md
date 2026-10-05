# LumaForge CDP Proxy

LumaForge CDP Proxy is an open-source Windows and Linux proxy, loader, and plugin
runtime for extending the Steam client through Chrome DevTools Protocol (CDP) and
Chromium Embedded Framework (CEF) integration.

> [!WARNING]
> LumaForge is currently in alpha development. Features may be incomplete, unstable, or changed without notice.

## Latest release

Current release: **v0.4.1**

Download published builds from the [GitHub Releases](https://github.com/eisora08/lumaforge-cdp-proxy/releases) page.
Every release publishes two archives:

- `lumaforge-proxy-windows-x64.zip` — Windows x64
- `lumaforge-proxy-linux-i686.zip` — Linux (Steam runtime, 32-bit)

## Features

- Detects Steam WebHelper process creation and injects a configurable Chrome DevTools
  Protocol debugging port with dynamic port selection and fallback.
- Injects JavaScript into every Steam CEF surface (store, library, popups) with
  re-injection on navigation and URL-change detection.
- HTTP bridge server (port `21775`) backing the injected UI: downloads queue,
  providers, Steam key manifest pins, game fixes, and the unified catalog/artwork
  endpoints (`/api/catalog`, `/api/art/{id}`).
- Mixed-content bridge proxy: page-side `fetch()` calls to the local bridge are
  queued and drained by the proxy so HTTPS store pages can reach HTTP endpoints.
- Millennium-compatible theme engine: `skin.json` parsing, default patches, system
  accent colors, `active.json` theme state, popup patching, and `window.Millennium`
  compatibility shims so existing Steam themes work unmodified.
- Lua plugin runtime for Steam Store extensions (see
  [lumaforge-extensions](https://github.com/eisora08/lumaforge-extensions)).
- Cross-platform loading — no `LD_PRELOAD` required:
  - Windows: `wsock32.dll` bootstrap next to `steam.exe` loads `lumaforge\lumaforge.dll`,
    which loads the CEF hook.
  - Linux: a `libXtst.so.6` proxy in `ubuntu12_32/` forwards XTest calls and loads
    `liblumaforge.so`.
- Publishes CDP discovery information (`steam-cdp.json`) for LumaForge.
- Structured logging and diagnostics for development and testing.

## Project status

LumaForge CDP Proxy is under active development. The plugin APIs, package
installation pipeline, CEF integration, and runtime behavior may change between
alpha releases.

## Requirements

- **Windows:** Windows 10 or 11 (x64) with Steam. Release archives from v0.4.1
  onwards are self-contained — no extra runtimes are required. Builds up to
  v0.4.0 load `vcruntime140.dll` from the system, so on machines that do not
  have it yet they require the
  [Visual C++ Redistributable 2015–2022 (x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe).
- **Linux:** Steam for Linux, installed as described under Installation.

## Installation

### Windows

1. Close Steam completely.
2. Download `lumaforge-proxy-windows-x64.zip` from [GitHub Releases](https://github.com/eisora08/lumaforge-cdp-proxy/releases).
3. Extract the archive contents directly into your Steam installation folder
   (the directory that contains `steam.exe`), so that:
   - `wsock32.dll` sits next to `steam.exe`
   - the `lumaforge\` folder sits next to `steam.exe`
4. Start Steam normally.

> [!IMPORTANT]
> `wsock32.dll` is part of the LumaForge loading mechanism. Do not copy it into
> `System32`, `SysWOW64`, or unrelated application directories. To disable
> LumaForge, rename it to `wsock32.dll.bak` and restart Steam.

### Linux

1. Close Steam completely.
2. Download `lumaforge-proxy-linux-i686.zip` from [GitHub Releases](https://github.com/eisora08/lumaforge-cdp-proxy/releases).
3. Copy `ubuntu12_32/liblumaforge.so` and `ubuntu12_32/libXtst.so.6` into
   `~/.local/share/Steam/ubuntu12_32/` (back up the originals first).
4. Start Steam normally — Steam loads `libXtst.so.6` automatically, so no
   `LD_PRELOAD` is needed.

## Release package

A Windows release archive contains:

```text
wsock32.dll
lumaforge\lumaforge.dll
lumaforge\lumaforge_cef_hook.dll
README.md
LICENSE
LICENSE.md
THIRD_PARTY_NOTICES.md
```

A Linux release archive contains:

```text
ubuntu12_32\liblumaforge.so
ubuntu12_32\libXtst.so.6
```

Build artifacts such as `.lib`, `.exp`, `.pdb`, `.d`, Cargo dependency folders,
and incremental build files are not part of the user release.

## Configuration

LumaForge configuration and plugin data are stored under the local LumaForge
application-data directory (`%LOCALAPPDATA%\LumaForge` on Windows,
`~/.local/share/LumaForge` on Linux). Configuration formats and available
settings may change during alpha development.

## Logging

Runtime logs cover the loader, CEF hook, CDP connection, plugins, providers, and
package installation pipeline. All logs live in the LumaForge runtime directory
(`%LOCALAPPDATA%\LumaForge\runtime` on Windows, `~/.local/share/LumaForge/runtime`
on Linux): `steamcdp_proxy.log`, `cef_hook.log`, `theme.log` and `steam-cdp.json`
sit side by side. Every proxy log line starts with a local `[HH:MM:SS.mmm]`
timestamp.

When reporting an issue, do not include API keys, authorization headers, private
configuration values, or other sensitive information.

## Build

### Windows requirements

- Windows 10 or Windows 11, x64
- Rust toolchain installed through `rustup`
- Cargo
- Visual Studio 2022 Build Tools with the MSVC x64 toolchain
- Windows SDK
- Git

The repository includes `rust-toolchain.toml`. Install the required toolchain and
components with:

```powershell
rustup show
rustup update
```

Quick build from the repository root:

```powershell
cargo build --release
```

### Linux requirements

- Rust toolchain with the `i686-unknown-linux-gnu` target
- 32-bit build toolchain and OpenSSL headers (Fedora/Nobara):

```bash
sudo dnf install -y gcc-multilib openssl-devel.i686 pkg-config
# Debian/Ubuntu:
sudo apt-get install -y gcc-multilib pkg-config libssl-dev:i386
```

Build the 32-bit proxy (the build script also builds the hook subcrate):

```bash
CC_i686_unknown_linux_gnu="gcc -m32" OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib/i386-linux-gnu \
  OPENSSL_INCLUDE_DIR=/usr/include \
  cargo build --release --target i686-unknown-linux-gnu
```

### Validation

Before publishing a release, run:

```powershell
cargo fmt --check
cargo check
cargo test
cargo build --release
```

Releases are published by pushing a `v*` tag; `.github/workflows/release.yml`
builds both platforms and attaches the zips to a GitHub Release.

### Output

Windows artifacts:

```text
target\release\lumaforge.dll
cef_hook\target\release\lumaforge_cef_hook.dll
bootstrap\wsock32.dll
```

Linux artifacts:

```text
target\i686-unknown-linux-gnu\release\liblumaforge.so
hook\target\i686-unknown-linux-gnu\release\libXtst.so
```

Debug symbols such as `.pdb` files may be published separately in an optional
symbols archive.

## Updating

1. Close Steam and LumaForge completely.
2. Back up custom plugins or configuration if necessary.
3. Download the new release archive.
4. Replace the previous loader and DLL/shared-object files with the new versions.

## Uninstallation

### Windows

1. Close Steam completely.
2. Rename `wsock32.dll` to `wsock32.dll.bak` (or delete it) in the Steam folder.
3. Remove the `lumaforge\` folder.
4. Start Steam normally.

### Linux

1. Close Steam completely.
2. Remove `liblumaforge.so` and restore the original `libXtst.so.6` from your backup.
3. Start Steam normally.

## Troubleshooting

If Steam does not start or a plugin fails to load:

1. Confirm that Steam was closed completely before launching LumaForge.
2. Confirm that the loader and DLL/shared-object files are in the documented
   locations.
3. Check whether antivirus software quarantined a release file.
4. Review the generated logs for loader, CEF hook, CDP, provider, or plugin errors.
5. Restore the previous release if the problem continues.
6. Include the LumaForge version, Windows/Linux version, Steam version, relevant
   logs, and reproduction steps when opening an issue.

### Which CDP transport is used

On Windows the proxy speaks DevTools over LumaForge-injected **pipes**;
the injected TCP port is currently disabled by default while the pipe path
is validated:

1. `lumaforge.dll` spawns the webhelper with `--remote-debugging-pipe` /
   `--remote-debugging-io-pipes` and an inheritable handle pair declared via
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, so no debug port needs to bind.
2. `--remote-debugging-port` is not injected unless `LUMAFORGE_CDP_TCP=1` is
   set (or Steam provides its own port, which is adopted as-is). Without a
   port the webhelper command line carries only the pipe flags.
3. `steam-cdp.json` is written only after a real TCP connection succeeds, so
   pipe-only sessions never advertise a port nobody listens on.
4. `lumaforge_cef_hook.dll` keeps its own chain: the port from its launch
   arguments first, then the local relay on `127.0.0.1:21778` that bridges to
   the shared pipe session. The relay retries its bind for a few seconds, so
   a busy port from a previous session does not disable CEF hook injection
   for the whole session.

Environment switches (set before starting Steam):

- `LUMAFORGE_CDP_TCP=1` — re-enable the injected `--remote-debugging-port`
  transport (TCP loop, discovery publishing). `=0` keeps it off explicitly;
  unset uses the current default (off). A session with registered pipes never
  falls back to TCP: the pipe watch loop waits for the helper to (re)spawn
  with fresh pipes instead (Millennium parity).
- `LUMAFORGE_NO_CDP_PIPES=1` — ignore the injected pipe flags and use TCP
  only (previous-release behavior).
- `LUMAFORGE_PIPE_PROBE=1` — verbose logging while probing spawned pipes.

### Steam starts but there is no log file

The proxy log lives in the LumaForge runtime directory:
`%LOCALAPPDATA%\LumaForge\runtime\steamcdp_proxy.log` on Windows and
`~/.local/share/LumaForge/runtime/steamcdp_proxy.log` on Linux, next to
`cef_hook.log` and `theme.log`. Logging is buffered: the file appears once
around ten lines have been written or after Steam exits cleanly from the tray.

If `%LOCALAPPDATA%\LumaForge\runtime\` was never created, `lumaforge.dll`
never loaded:

1. Look for `lumaforge\load_error.txt` next to the DLL — it records the
   loader's `GetLastError` (`126` = missing file or runtime dependency,
   `193` = wrong architecture).
2. Confirm the files sit in the Steam folder of the *running* `steam.exe`
   (Task Manager → Open file location) and restart Steam from the tray.
3. Let the files through antivirus and unblock them:
   `Unblock-File wsock32.dll` and `Unblock-File lumaforge\lumaforge.dll`.

### CDP connection fails with `10061` (connection refused)

The debug port was injected but nothing accepted the connection yet:

1. Check `%LOCALAPPDATA%\LumaForge\runtime\cef_hook.log`: `Using port from
   args` means the flag reached the webhelper; repeated
   `Could not get browser WebSocket URL` means CEF has not started its DevTools
   server yet (recent builds keep retrying until it does).
2. While Steam is running, run `netstat -ano | findstr <port>` with the port
   from `%LOCALAPPDATA%\LumaForge\runtime\steam-cdp.json`. `LISTENING`
   appearing only after the first failures means CEF started slower than the
   client probed. If `netstat` never shows the port but
   `%LOCALAPPDATA%\LumaForge\runtime\steamcdp_proxy.log` reports
   `Pipe CDP connected`, that is expected: the session runs over inherited
   pipes and the CEF hook reaches it through the relay on port 21778.
3. `netsh interface ipv4 show excludedportrange protocol=tcp` — a port inside
   an excluded range cannot be bound by CEF; set the `STEAMCDP_PORT`
   environment variable to a port outside those ranges to force one.
4. Some antivirus products filter loopback connections; try excluding the
   Steam folder.

Report problems through [GitHub Issues](https://github.com/eisora08/lumaforge-cdp-proxy/issues).

## Security

Download release binaries only from the official repository:

<https://github.com/eisora08/lumaforge-cdp-proxy>

Do not download modified binaries from unknown third-party sources. Never publish
API keys, authorization headers, private configuration files, or sensitive logs in
issue reports.

## License

LumaForge CDP Proxy is distributed under the MIT License. See [`LICENSE.md`](LICENSE.md) for details.

Copyright (c) 2026 eisora08

## Third-party software

This project includes third-party open-source components and derived code —
notably the theme pipeline ported from [Millennium](https://github.com/SteamClientHomebrew/Millennium).
See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) or the notices included in
a release for the applicable licenses and acknowledgements.

## Disclaimer

This project is provided for research and educational purposes only. You are
responsible for complying with applicable local laws, platform terms of service,
and software licenses.

LumaForge is an independent project and is not affiliated with, endorsed by,
sponsored by, or associated with Valve Corporation or Steam.
