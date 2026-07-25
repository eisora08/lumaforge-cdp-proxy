# LumaForge CDP Proxy

LumaForge CDP Proxy is an experimental Windows proxy, launcher, and plugin runtime for extending the Steam client through Chrome DevTools Protocol (CDP) and Chromium Embedded Framework (CEF) integration.

> [!WARNING]
> LumaForge is currently in alpha development. Features may be incomplete, unstable, or changed without notice.

## Latest release

Current development release: **v0.2.0-alpha.1**

Download published builds from the [GitHub Releases](https://github.com/eisora08/lumaforge-cdp-proxy/releases) page.

## Features

- Detects Steam WebHelper process creation.
- Injects a configurable Chrome DevTools Protocol debugging port.
- Supports dynamic port selection and fallback behavior.
- Publishes CDP discovery information for LumaForge.
- Provides a dedicated launcher for starting Steam with LumaForge.
- Supports CEF and CDP integration.
- Provides infrastructure for Steam client plugins.
- Includes provider-based package management and automatic update infrastructure.
- Includes structured logging and diagnostics for development and testing.

## Project status

LumaForge CDP Proxy is under active development. The plugin APIs, package installation pipeline, CEF integration, and runtime behavior may change before the stable `v0.2.0` release.

## Installation

1. Close Steam completely.
2. Download the latest Windows archive from [GitHub Releases](https://github.com/eisora08/lumaforge-cdp-proxy/releases).
3. Extract the entire archive into a dedicated directory.
4. Keep `lumaforge_cef_hook.dll`, and `user32.dll` together.


> [!IMPORTANT]
> The included `user32.dll` is part of the LumaForge loading mechanism. Do not copy it into `System32`, `SysWOW64`, or unrelated application directories.

## Release package

A typical Windows release contains:


- `lumaforge_cef_hook.dll`
- `user32.dll`
- `README.txt`
- `LICENSE.txt`
- `THIRD_PARTY_NOTICES.txt`

Build artifacts such as `.lib`, `.exp`, `.pdb`, `.d`, Cargo dependency folders, and incremental build files are not required in the standard user release.

## Configuration

LumaForge configuration and plugin data may be stored under the local LumaForge application-data directory. Configuration formats and available settings may change during alpha development.

## Logging

Runtime logs are available for troubleshooting the launcher, CEF hook, CDP connection, plugins, providers, and package installation pipeline.

Log verbosity is controlled by the logging configuration supported by LumaForge. Configuration names and locations may change during alpha development.

When reporting an issue, do not include API keys, authorization headers, private configuration values, or other sensitive information.

## Build

### Requirements

- Windows 10 or Windows 11, x64
- Rust toolchain installed through `rustup`
- Cargo
- Visual Studio 2022 Build Tools with the MSVC x64 toolchain
- Windows SDK
- Git

### Rust toolchain

The repository includes `rust-toolchain.toml`. Install the required toolchain and components with:

```powershell
rustup show
rustup update
```

### Quick build

From the repository root, run:

```powershell
cargo build --release
```

### Validation

Before publishing a release, run:

```powershell
cargo fmt --check
cargo check
cargo test
cargo build --release
```

### Output

Release artifacts are generated under:

```text
target\release\
```

The user-facing release normally includes:

```text
target\release\lumaforge_cef_hook.dll
target\release\user32.dll
```

Debug symbols such as `.pdb` files may be published separately in an optional symbols archive.

## Updating

1. Close Steam and LumaForge completely.
2. Back up custom plugins or configuration if necessary.
3. Download the new release archive.
4. Replace the previous executable and DLL files with the new versions.

## Uninstallation

1. Close Steam completely.
2. Remove the LumaForge files from the installation directory.
3. Restore any original files that were manually replaced.
4. Start Steam normally.

## Troubleshooting

If Steam does not start or a plugin fails to load:

1. Confirm that Steam was closed completely before launching LumaForge.
2. Confirm that all release executables and DLL files remain together.
3. Check whether antivirus software quarantined a release file.
4. Review the generated logs for hook, CDP, provider, or plugin errors.
5. Restore the previous release if the problem continues.
6. Include the LumaForge version, Windows version, Steam version, relevant logs, and reproduction steps when opening an issue.

Report problems through [GitHub Issues](https://github.com/eisora08/lumaforge-cdp-proxy/issues).

## Security

Download release binaries only from the official repository:

<https://github.com/eisora08/lumaforge-cdp-proxy>

Do not download modified binaries from unknown third-party sources. Never publish API keys, authorization headers, private configuration files, or sensitive logs in issue reports.

## License

LumaForge CDP Proxy is distributed under the MIT License. See [`LICENSE`](LICENSE) for details.

Copyright (c) 2026 eisora08

## Third-party software

This project may include third-party open-source components. See `THIRD_PARTY_NOTICES.md` or the notices included in a release for the applicable licenses and acknowledgements.

## Disclaimer

This project is provided for research and educational purposes only. You are responsible for complying with applicable local laws, platform terms of service, and software licenses.

LumaForge is an independent project and is not affiliated with, endorsed by, sponsored by, or associated with Valve Corporation or Steam.
