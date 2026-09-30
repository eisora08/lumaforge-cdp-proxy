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
package installation pipeline. Logs are written to the LumaForge log directory;
on Linux the CDP proxy log is `/tmp/steamcdp_proxy.log`.

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
