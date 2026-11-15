<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://tux-force.github.io/brand/logo-horizontal-dark-bg.png" />
    <img src="https://tux-force.github.io/brand/logo-horizontal-light-bg.png" alt="Tux-Force" width="420" />
  </picture>
</p>

<h1 align="center">xTux</h1>
<p align="center">Play the PC games you own or subscribe to on Linux — with real licenses, no cracks.</p>
<p align="center"><a href="https://tux-force.github.io">Website</a> · <a href="https://github.com/Tux-Force">Tux-Force</a> · soft fork of <a href="https://github.com/xodus-gaming/xodus">Xodus</a></p>

> [!CAUTION]
> This is an unofficial community project - use at your own risk. It is not affiliated with, endorsed by, or sponsored
> by Microsoft or Xbox; all trademarks, product names, and company names or logos mentioned herein are the property
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

## Using xTux

Guides, the compatibility list and downloads are on the website, **https://tux-force.github.io**:

- [Quick start](https://tux-force.github.io/guide/quick-start/) and [installation](https://tux-force.github.io/guide/installation/)
- [Running games](https://tux-force.github.io/guide/running-games/): `xtux-cli list`, `run` by name, Windows installs, `migrate`
- [Sign-in and password prompts](https://tux-force.github.io/guide/sign-in/), [troubleshooting](https://tux-force.github.io/guide/troubleshooting/)
- [Compatibility](https://tux-force.github.io/compatibility/) and [architecture](https://tux-force.github.io/project/architecture/)

`xtux-cli --help` lists all commands. The command line tool is **xTux-cli** (`xtux-cli`), the daemon **xTux-service**
(`xtux-service`) and the graphical front end the **xTux Launcher** (`xtux-launcher`); the core library keeps its
upstream name `xodus`.

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
> xtux-service aims to become a main point of integration. All xodus clients will connect to it to interact with games and Xbox services.

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

## License

xTux is free software under the GNU General Public License 3.0 ([LICENSE](LICENSE)); the crates taken over from
Xodus are `GPL-3.0-only`, the new xTux crates `GPL-3.0-or-later`. The licenses of all dependencies are listed in
[licenses/THIRD-PARTY-LICENSES.md](licenses/THIRD-PARTY-LICENSES.md) (generated with `scripts/third-party-licenses.sh`,
shipped with every release and shown on the launcher's About page). The CI checks with `cargo deny` (`deny.toml`)
that every dependency has a GPL-3.0 compatible license and comes from a known source. More on the website:
[Licenses](https://tux-force.github.io/project/licenses/).

## Special Thanks

- The [Xodus](https://github.com/xodus-gaming/xodus) team and contributors, on whose work xTux is built

- [XvdTool.Streaming](https://github.com/LukeFZ/XvdTool.Streaming) and [CikExtractor](https://github.com/LukeFZ/CikExtractor) by LukeFZ
