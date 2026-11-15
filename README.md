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

- **One install format:** xTux reads and writes the layout of the Windows Xbox app (`Content/` plus the package
  header named by its content id), so `xtux-cli run` starts games straight from a Windows installation (dual boot,
  NTFS) without a second download. `xtux-cli migrate <folder>` moves an older Xodus install into that layout.
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

The command line tool is **xTux-cli** (`xtux-cli`) and the daemon **xTux-service** (`xtux-service`); the core library
keeps its upstream name `xodus`.

## Quick start

```bash
# sign in once
xtux-cli login
# prepare a prefix: DXVK, vkd3d-proton and the Microsoft root certificates games expect
xtux-cli prefix init /path/to/tux-force-wine/wine --prefix ~/Games/xtux-prefix
# run a game installed by the Windows Xbox app, or a folder downloaded with xtux-cli
WINEPREFIX=~/Games/xtux-prefix xtux-cli run -m DE "/path/to/XboxGames/<Game>" /path/to/tux-force-wine/wine
# or by name, with Wine, prefix and market from ~/.config/xtux/settings.toml
xtux-cli list
xtux-cli run "Farming Simulator 25 (PC)"
```

Shell completion (game names for `run`): add `source <(COMPLETE=zsh xtux-cli)` to `~/.zshrc`, or
`source <(COMPLETE=bash xtux-cli)` to `~/.bashrc`.

`run` and `prefix init` go through the daemon `xtux-service`, which `xtux-cli` starts on demand and which exits
after five idle minutes (`XTUX_IDLE_TIMEOUT` in seconds, `0` keeps it running). Its log, including the game bridge, is
in `~/.local/state/xtux/service.log`. `XTUX_DIRECT=1` runs a command inside `xtux-cli` instead, without the daemon.

Two password prompts can appear, neither from xTux itself:

- **Keychain** (KWallet, GNOME Keyring): xTux keeps its Microsoft device and sign-in tokens there, so a locked keychain
  asks for its password. If it stays locked, xTux stops instead of registering the PC again.
- **polkit, once:** registering the PC with Microsoft reads the firmware system information
  (`/sys/firmware/dmi`, root only), as Windows sends it too. xTux explains this first in a dialog of its own (kdialog or zenity) with
  Continue and Skip; cancelling is fine, and `XTUX_SKIP_SMBIOS=1` skips it.

`prefix init` downloads DXVK and vkd3d-proton from their GitHub releases (or takes them from a local Proton build with
`--from-proton`), keeps Wine's own DLLs in `xtux-builtin-backup/` and records the setup in `xtux-prefix.json`, from
which `run` takes the DLL overrides.

## Building

The project structure is as follows.

```
.
├── msixvc - [rlib] common rlib crate for utilities for parsing MSIXVC and XSP files
├── xodus - [rlib] common rlib crate that contains core xodus functionality, API calls abstractions and utilities
├── xtux-cli - [bin] xTux-cli, the command line front end
└── xtux-service - [bin] service process exposing xtux.sock for IPC communication, it takes care of xgameruntime.dll integration.
```

> [!NOTE]
> xtux-service aims to become a main point of integration. All xodus clients will connect to it to interact with games and XBOX services.

### Prerequisites

- Rust version 1.98 or later
- Right now CLI relies on wry and tao to show a login page. Consult https://docs.rs/wry/latest/wry/#platform-considerations
- xtux-service relies on `protoc` to compile `proto/` definitions make sure to install it for your platform

### Running

Building all crates in release mode

```bash
cargo build --release --workspace
```

Running cli in debug

```
cargo run -- --help
```

Running xtux-service in debug

```
cargo run --bin xtux-service
```

Debug and profile `xtux-cli` or `xtux-service` with [tokio-console](https://github.com/tokio-rs/console)

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
Usage: xtux-cli <COMMAND>

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
