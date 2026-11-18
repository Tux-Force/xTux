# xTux v0.2.0

This release makes xTux easier to play with: games install in the background, Windows Xbox app installs can be updated, a game that ends abnormally explains why, and Minecraft for Windows runs with keyboard, mouse and controllers.

## Included changes

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

- Use the Tux-Force Wine build released together with this version (`wine-…` release of [Tux-Force/wine](https://github.com/Tux-Force/wine/releases)). It contains xgameruntime and the Wine changes Minecraft and GameInput need (package identity, WNF and stubless proxy exports). With an older Wine, GameInput's service does not start and Minecraft asks for a missing component.
- xTux does not download Wine yet: set the Wine binary in Settings → Games, then run `xtux-cli prefix init` once.
- A controller mode change applies once all games of the prefix are closed, because Wine reads it when the prefix starts.
- DXVK/vkd3d, WebKitGTK/GTK and the other desktop libraries remain external requirements, as in v0.1.0.

## Validation

- `cargo fmt --all --check`, `cargo test --workspace`, `cargo clippy`, `cargo deny check advisories licenses bans sources`; third-party license list regenerated.
- Minecraft for Windows from a Windows Xbox app install: starts through `xtux-cli run`, the GameInput redistributable and controller mode were set up automatically in a prefix without them, no missing-component dialog; keyboard, mouse and a hot-plugged DualSense confirmed in game.
- Microsoft Flight Simulator 2024: update of a Windows Xbox app install left unfinished by the Xbox app, game started afterwards (see #14).

## Automated publication

Pushing `v0.2.0` on `main` triggers the CI/CD pipeline, which builds, tests and checks licenses, then publishes the release with the CI-built archives and SHA-256 files. 0.x versions are published as prereleases.
