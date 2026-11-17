# xTux v0.1.0 release candidate

The first xTux release includes the graphical launcher, CLI and service for managing a local game library, browsing owned and PC Game Pass titles, installing supported GDK packages and starting games with the configured Tux-Force Wine build.

## Included changes

- Owned downloadable PC DLCs appear on their game's Store page, with install, download-queue and removal actions. Removal requires confirmation of the exact DLC folder.
- Manual game and DLC update checks show installed and offered package versions. Supported xTux-managed installations can be updated in place through the persistent download queue.
- Pause, resume, cancellation and retries preserve existing update installations. Incomplete updates block launches until completed. Update journals belong to the exact package, retired package files are removed, and unrelated user files remain.
- Process groups allow Stop to force unresponsive launch children to exit. Games start from their Content directory and use the desktop executable declared by the game manifest.
- Certificate-only prefix updates preserve graphics overrides and existing DXVK/vkd3d settings.
- Companion runtime fixes implement Unicode Xbox token requests and correct the missing-blob GDK save error contract. Among Us authentication and Digging were confirmed working by the user with the tested runtime/prefix.
- The companion Wine fix reads executable image information from decrypted memory mappings when the file on disk remains encrypted.

## Requirements and limitations

- Published archives are built and tested by GitHub Actions on Ubuntu Linux x86-64. Wine, DXVK/vkd3d and the desktop's WebKitGTK/GTK/system libraries remain external requirements.
- The runtime and Wine fixes are separate repository commits: [runtime 03bd2a6](https://github.com/Tux-Force/xgameruntime/commit/03bd2a68b596b660345994d9f6cf99ed6d3f265d) and [Wine ea05fb0](https://github.com/Tux-Force/wine/commit/ea05fb0193cdec23102e8b075fd36eba269aab1a). A matching runtime/Wine setup is required for those compatibility improvements; the launcher archive alone does not replace Wine or a prefix DLL.
- Windows Xbox app installations can be checked for updates, but must be updated through Windows. xTux does not rewrite Xbox app bookkeeping files.
- DLC management covers listing, downloads and removal. The runtime's in-game package discovery/mounting support is not complete.
- Microsoft Flight Simulator 2024 remains unsupported in the current test installation. Its local installed container representation fails direct XVD metadata parsing; earlier tests also showed a process warning and native crash. No repair is included in this release candidate.
- Stop handles the launch's process group; detached Wine processes may outlive it.

## Validation

- `cargo fmt --all --check`
- `cargo build --release --workspace --locked --offline`
- `cargo test --release --workspace --locked --offline` (local HTTP test servers enabled)
- `cargo deny --offline check licenses sources bans advisories`
- Generated third-party license list matches the committed list.
- Native runtime Unicode-token tests pass against a local fake bridge in an isolated Wine prefix.
- Genuine account checks correctly identify Minecraft's available update and distinguish current packages from Windows-managed update targets. No real game update was installed as part of release validation.

See [Updates](Updates.md) for update behavior and CLI commands.

## Automated publication

Pushing `v0.1.0` triggers the CI/CD pipeline. Only successful build/tests and license/advisory checks permit publication. The pipeline builds and verifies the binary archive, source archive and SHA-256 files, then creates the GitHub release and uploads these CI-built assets automatically. A retry uploads missing/rebuilt assets to the existing release. Tags must match the CLI package version and have matching release notes. Tags for 0.x versions or with a prerelease suffix are published as prereleases. Branch pushes and manual check runs do not publish releases.
