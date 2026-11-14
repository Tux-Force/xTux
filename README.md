<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/Tux-Force/tux-force/main/brand/png/logo-horizontal-dark-bg.png" />
    <img src="https://raw.githubusercontent.com/Tux-Force/tux-force/main/brand/png/logo-horizontal-light-bg.png" alt="Tux-Force" width="420" />
  </picture>
</p>

<h1 align="center">xTux</h1>
<p align="center">Play the PC games you own or subscribe to on Linux — with real licenses, no cracks.</p>
<p align="center"><a href="https://github.com/Tux-Force">Tux-Force</a> · soft fork of <a href="https://github.com/xodus-gaming/xodus">Xodus</a></p>

> [!CAUTION]
> This is an unofficial community project - use at your own risk. It is not affiliated with, endorsed by, or sponsored
> by Microsoft or XBOX; all trademarks, product names, and company names or logos mentioned herein are the property
> of their respective owners.

## What is xTux?

xTux is the core of [Tux-Force](https://github.com/Tux-Force): it signs you in with your Microsoft account, fetches
licenses from the Microsoft licensing service, downloads or reuses game packages and starts GDK games in Wine.
Only games your account is entitled to run; there is no license bypass.

xTux is a **soft fork of [Xodus](https://github.com/xodus-gaming/xodus)** by the xodus-gaming community. Everything
here builds on their work. We pull in new upstream code when it is useful; otherwise xTux develops independently
to move faster on everything a complete Linux experience needs. Tux-Force is built with AI and welcomes AI-assisted
contributions (see [CONTRIBUTING](https://github.com/Tux-Force/.github/blob/main/CONTRIBUTING.md)).

### What xTux adds on top of Xodus

- **Shared installs:** `xodus-cli run` and `xodus-cli link` start games straight from an installation made by the
  Windows Xbox app (dual boot, NTFS), no second download.
- **Bridge for GDK services:** `run` starts a loopback service with a per-run secret that answers what the game's
  xgameruntime needs from Microsoft without handing account tokens to the game:
  - store license tokens (`XStoreQueryLicenseToken`),
  - title-bound, signed Xbox Live tokens (device, title and user token, as on Windows), with relying parties from the
    title's own endpoint table.
- **Game start fixes:** software video decoders for intro videos, identity and license state for xgameruntime.
- **Merged upstream pull requests** that are not in Xodus `main` yet (#216, #237, #242, #243, #217, #212, #197, #215).

The matching runtime and Wine changes live in [Tux-Force/xgameruntime](https://github.com/Tux-Force/xgameruntime)
and [Tux-Force/wine](https://github.com/Tux-Force/wine).

## Current state

| Game | State |
|---|---|
| Farming Simulator 25 | Plays: menus, saves, ModHub, Xbox Live (achievements, presence, real-time) and clean exit |
| Microsoft Flight Simulator 2024 | Signs in and reaches its services, then fails on its own backend (under investigation) |

Binaries and crates still carry the Xodus names (`xodus-cli`, `xodus-service`) to keep merging upstream easy.

## Quick start

```bash
# sign in once
xodus-cli login
# prepare a prefix: DXVK, vkd3d-proton and the Microsoft root certificates games expect
xodus-cli prefix init /path/to/tux-force-wine/wine --prefix ~/Games/xtux-prefix
# run a game installed by the Windows Xbox app, or a folder downloaded with xodus-cli
WINEPREFIX=~/Games/xtux-prefix xodus-cli run -m DE "/path/to/XboxGames/<Game>" /path/to/tux-force-wine/wine
```

`prefix init` downloads DXVK and vkd3d-proton from their GitHub releases (or takes them from a local Proton build with
`--from-proton`), keeps Wine's own DLLs in `xtux-builtin-backup/` and records the setup in `xtux-prefix.json`, from
which `run` takes the DLL overrides.

## Building

The project structure is as follows.

```
.
├── msixvc - [rlib] common rlib crate for utilities for parsing MSIXVC and XSP files
├── xodus - [rlib] common rlib crate that contains core xodus functionality, API calls abstractions and utilities
├── xodus-cli - [bin] CLI currently used for iterating over new xodus features
└── xodus-service - [bin] service process exposing a xodus.sock for IPC communication, it takes care of xgameruntime.dll integration.
```

> [!NOTE]
> xodus-service aims to become a main point of integration. All xodus clients will connect to it to interact with games and XBOX services.

### Prerequisites

- Rust version 1.98 or later
- Right now CLI relies on wry and tao to show a login page. Consult https://docs.rs/wry/latest/wry/#platform-considerations
- xodus-service relies on `protoc` to compile `proto/` definitions make sure to install it for your platform

### Running

Building all crates in release mode

```bash
cargo build --release --workspace
```

Running cli in debug

```
cargo run -- --help
```

Running xodus-service in debug

```
cargo run --bin xodus-service
```

Debug and profile `xodus-cli` or `xodus-service` with [tokio-console](https://github.com/tokio-rs/console)

```
RUSTFLAGS="--cfg tokio_unstable" cargo run --features tokio_console 
```

> [!WARNING]
> For better performance when decrypting MSIXVC files, the `aes` and `ssse3` features are enabled on `x86_64`,
> and the `aes` feature is enabled on `aarch64`. This means that the program will crash with an illegal instruction
> error when running on a CPU which doesn't support those instructions.
>
> See https://en.wikipedia.org/wiki/AES_instruction_set for a list of compatible CPUs (every processor from
> 2011 onwards should be supported).

### CLI Usage

```
Usage: xodus-cli <COMMAND>

Commands:
  download    Download msixvc or xsp files for a given game
  license     Dump CIKs for use with XvdTool
  extract     Extract locally stored msixvc file
  login       
  streaming   Download and extract the game through streaming algorithm
  clep        Generate or decrypt base64-encoded CLEP challenge data
  sp-license  Decode SPLicenseBlock
  help        Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

## Special Thanks

- The [Xodus](https://github.com/xodus-gaming/xodus) team and contributors, on whose work xTux is built

- [XvdTool.Streaming](https://github.com/LukeFZ/XvdTool.Streaming) and [CikExtractor](https://github.com/LukeFZ/CikExtractor) by LukeFZ
