# xTux v0.3.0

This release brings Xbox cloud saves, native packages for Debian, Ubuntu, Fedora, openSUSE and Arch, a launcher that works with a controller, anti-cheat runtimes for Easy Anti-Cheat and BattlEye games, and DLC that games recognise without a connection.

## Included changes

- **Xbox cloud saves**: before a game starts, saves that changed in the Xbox cloud are brought in; after it ends, saves changed in xTux go up, as Gaming Services do on Windows.
  - Saves changed on both sides since the last sync: the newer one wins, the other one is kept in `~/.local/state/xtux/save-backups/`.
  - Saves that differ on both sides and were never synced are left alone until you pick a side.
  - A sync never deletes saves.
  - `xtux-cli saves list|pull|push|resolve <game> [cloud|local]`; "Cloud saves" switch in the settings (on by default).
  - Games whose SCID has no cloud storage (DayZ, Minecraft for Windows) are skipped.
- **Native packages**: every release ships `.deb` (Debian 13+, Ubuntu 24.04+), `.rpm` (Fedora 40+, openSUSE Tumbleweed) and `.pkg.tar.zst` (Arch) next to the archive and the Flatpak. Wine is not a dependency, xTux downloads its own. The packages are not signed and there is no apt/dnf repository yet.
- **Launcher with a controller**: Xbox and PlayStation pads over USB and Bluetooth move through tiles and buttons, open and start games and switch pages (L1/R1), with a focus ring and a hint bar drawn like the pad in use.
- **Anti-cheat**:
  - Easy Anti-Cheat games run with the Proton EasyAntiCheat Runtime found in the Steam libraries, when the game ships EAC's Linux client. Without the client or the runtime, the start says why online play will likely not work.
  - BattlEye games start through `belauncher` with the Proton BattlEye Runtime.
  - Reports from [AreWeAntiCheatYet](https://areweanticheatyet.com) on the game page, in the Store and in `xtux-cli list`, with how likely the report belongs to the game. They replace xTux's own title list.
- **DLC in games**: xgameruntime now answers games' add-on and license queries from the installed add-ons, so DLC installed through xTux is found and mounted by the game, also offline, and a game running notices a newly installed add-on.
- **Sign-in confirmation per game**: some games (ASTRONEER) need the sign-in confirmed once before Microsoft hands out Xbox Live tokens. xTux now says so instead of failing with `missing field wst:TokenType`; `xtux-cli login --game <game>` opens the confirmation in the sign-in window.
- **Launcher**: every path setting has a Browse button (desktop file dialog, also in the Flatpak), library folders are edited one per line, and the game and service logs are coloured by level.
- **Setup**: preparing a prefix in a folder whose parents do not exist yet works, and Wine's Mono and Gecko installer dialogs no longer appear.
- DayZ: game save and store UI fixes in xgameruntime; Wine's builtin AMD AGS is turned off for games that do not ship their own, which made DayZ overflow its stack.
- Dependency update: `url` 2.5.9 (RUSTSEC-2026-0592).

## Requirements and limitations

- The setup downloads the Tux-Force Wine released together with this version. It contains the xgameruntime with the DLC and save changes above; with an older Tux-Force Wine, games do not see their DLC.
- Online play with Easy Anti-Cheat needs the game's developer to have enabled EAC for Linux; BattlEye needs the Proton BattlEye Runtime (Steam → Library → Tools).
- The archive build needs WebKitGTK/GTK and the other desktop libraries of the system; the Flatpak and the native packages bring or require them.

## Validation

- `cargo fmt --all --check`, `cargo test --workspace`, `cargo clippy`, `cargo deny check advisories licenses bans sources`; third-party license list regenerated.
- Native packages installed with the distribution's package manager in Debian 13, Ubuntu 24.04, Fedora, openSUSE Tumbleweed and Arch containers; all libraries resolve, `xtux-cli` and `xtux-launcher` start.
- Cloud saves against the real service: Planet Coaster 2 saves uploaded and downloaded again byte for byte; Microsoft Flight Simulator 2024 logbooks downloaded; a real launch synced before and after the game.
- DLC: xgameruntime tests against a fake bridge (enumeration, licenses, mounting, new add-on while running).
- Sign-in: ASTRONEER reports the needed confirmation, Among Us signs in as before.

## Automated publication

Pushing `v0.3.0` on `main` triggers the CI/CD pipeline, which builds, tests and checks licenses, then publishes the release with the CI-built archive, Flatpak, native packages and SHA-256 files. 0.x versions are published as prereleases.
