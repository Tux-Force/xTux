# xTux v0.2.0

This release makes xTux easier to play with: a guided setup does everything from the Wine download to the prefix, xTux comes as a Flatpak, games install in the background, Windows Xbox app installs can be updated, a game that ends abnormally explains why, and Minecraft for Windows runs with keyboard, mouse and controllers.

## Included changes

- **Guided setup**: the launcher opens "Welcome to xTux" on the first start; "Set everything up" signs in (the only step that needs you), downloads the Tux-Force Wine, prepares the prefix and finds the game folders, including `XboxGames` folders of Windows drives below the home folder. `xtux-cli` without a command shows a home screen with the state and a menu (play, updates, downloads, setup) and starts the same setup in the terminal; `xtux-cli setup` runs it again. See [Setup](Setup.md).
- **Managed Wine**: xTux downloads the newest [Tux-Force Wine](https://github.com/Tux-Force/wine/releases) release, verifies its SHA-256 file and uses it when the settings name no Wine.
- **Flatpak**: `xtux-linux-x86_64.flatpak` on the GNOME runtime, which brings WebKitGTK for the sign-in, so nothing else has to be installed.
- **Download queue**: installs run one at a time in the background through `xtux-service`, also without the launcher open, and continue after a restart. Pause and resume keep finished files and continue partly downloaded files at their last page. `xtux-cli downloads` manages the queue.
- **Updates of Windows Xbox app installs**: only the package header and changed files are replaced; the Xbox app's own bookkeeping stays untouched. An update the Xbox app left unfinished is detected, launches are blocked until it is completed, and every file is checked page by page so that intact files are kept. New `xtux-cli verify <game folder>` checks an install against its package.
- **Crash message**: when a game ends abnormally, the launcher names the cause (license failure, out of graphics memory, GPU lost, missing DLL, unimplemented Wine function, unhandled exception, signal) with a hint and a button to open the log. Low free graphics memory shows a hint before a start.
- **Minecraft for Windows**:
  - games whose entry executable is not encrypted start from the game folder,
  - games depending on the Windows App Runtime get their Windows package identity, so they skip the runtime bootstrapper as on Windows,
  - the GameInput redistributable a game ships (`GameInputRedist.msi`) is installed into the prefix at launch.
- **Controllers**: new setting for PlayStation controllers (DualShock 4, DualSense, DualSense Edge), in general and per game. By default they reach games as Xbox gamepads, the kind XInput and GameInput games understand; "As PlayStation controller" keeps them as they are for games with their own PlayStation support.
- Launcher documentation and screenshots in the README.

## Requirements and limitations

- The setup downloads the Tux-Force Wine released together with this version. It contains xgameruntime and the Wine changes Minecraft and GameInput need (package identity, WNF and stubless proxy exports). With an own, older Wine, GameInput's service does not start and Minecraft asks for a missing component.
- A controller mode change applies once all games of the prefix are closed, because Wine reads it when the prefix starts.
- The archive build needs WebKitGTK/GTK and the other desktop libraries of the system; the Flatpak brings them. DXVK and vkd3d-proton are downloaded by the prefix setup.

## Validation

- Setup in the terminal with empty settings and a new prefix: settings saved, prefix prepared with DXVK, vkd3d-proton, certificates and controller mode. Wine download tested against a local server (checksum accepted and refused); the first real Tux-Force Wine release is published together with this version.
- Flatpak: built in CI from the release binaries; all libraries resolve in the runtime, `xtux-cli` starts inside it.
- `cargo fmt --all --check`, `cargo test --workspace`, `cargo clippy`, `cargo deny check advisories licenses bans sources`; third-party license list regenerated.
- Minecraft for Windows from a Windows Xbox app install: starts through `xtux-cli run`, the GameInput redistributable and controller mode were set up automatically in a prefix without them, no missing-component dialog; keyboard, mouse and a hot-plugged DualSense confirmed in game.
- Microsoft Flight Simulator 2024: update of a Windows Xbox app install left unfinished by the Xbox app, game started afterwards (see #14).

## Automated publication

Pushing `v0.2.0` on `main` triggers the CI/CD pipeline, which builds, tests and checks licenses, then publishes the release with the CI-built archives and SHA-256 files. 0.x versions are published as prereleases.
