# xTux v0.4.0

This release brings Xbox achievements, the system dialogs of games, in-game events, the Store inside games, and a dialog for cloud saves that conflict.

## Included changes

- **Achievements**: the game page and `xtux-cli achievements <game>` show the game's Xbox achievements, progress and Gamerscore. New unlocks appear as a desktop notification while a game runs (Settings → Achievement notifications). xTux never unlocks anything itself.
- **Game UI** (xgameruntime): message, error and text entry dialogs, achievements and profile cards, gamer pictures, and the game's own UI through `XGameUiSetUiCallbacks`.
- **In-game events**: `XGameEventWrite` events are uploaded in order, tried again on failure and flushed when the game ends.
- **Store in games**: games get their products with prices and ownership, consumables and Store ID keys. A game only sees and changes the ownership of its own products.
- **Cloud save conflicts in the launcher**: saves that differ here and in the Xbox cloud and were never synced are shown on the game page, and a dialog keeps either side per save or for all. The other side is backed up. A save that changed in the meantime is refused, and so is settling while the game runs. `xtux-cli saves resolve` works as before.
- **Bridge**: a new sign-in when Xbox refuses the old one; redirects of gamer pictures stay on known hosts.
- Dependency update: `cookie` 0.18.3 (RUSTSEC-2026-0594).

## Requirements and limitations

- The game UI, events and Store need the Tux-Force Wine released together with this version, which contains the matching xgameruntime.
- Invites, the player picker, web authentication and game streaming or capture functions of the GDK are not implemented.
- **Not verified with a real account and game**: the event upload (its ticket headers are derived, the upload library is not open), ownership in the new Store lists, consuming purchases, Store ID keys, gamer pictures since their conversion to PNG, the notification on a real unlock, and settling a real save conflict. Consuming a purchase is final.

## Validation

- `cargo fmt --all --check`, `cargo test --workspace`, `cargo clippy`, `cargo deny check advisories licenses bans sources` in the CI.
- Achievement lists of Planet Coaster 2, Minecraft, DayZ, ASTRONEER and Flight Simulator 2024 from the real service; product list against the public Store.
- xgameruntime tests against a fake bridge: Store and events, add-ons, game UI, tokens.
- The save conflict dialog was checked in snapshots with made-up data.
- macOS is not a release target; its CI check has had no result since the game UI change because the runners cancelled it.

## Automated publication

Pushing `v0.4.0` on `main` triggers the CI/CD pipeline, which builds, tests and checks licenses, then publishes the release with the CI-built archive, Flatpak, native packages and SHA-256 files. 0.x versions are published as prereleases.
