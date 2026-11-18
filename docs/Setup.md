# Setup

xTux sets itself up. The first start of the launcher opens **Welcome to xTux**, and `xtux-cli` without a command
starts the same steps in the terminal when something is missing. Every step checks what is already in place, so the
setup can run again at any time (launcher: Settings → Games → **Run the setup…**, terminal: `xtux-cli setup`).

## The steps

1. **Microsoft account**: sign in with the account that owns the games or has PC Game Pass. Microsoft's sign-in page
   opens in its own window.
2. **Wine**: xTux downloads the newest [Tux-Force Wine](https://github.com/Tux-Force/wine/releases) release, checks
   it against its SHA-256 file and unpacks it into `~/.local/share/xtux/wine/<release>`; `current` links to the
   version in use and older versions are removed. With no Wine in the settings, games start with this one. To use
   another Wine, choose **Use my own Wine** (launcher) or "Use a Wine I already have" (terminal); the path is then
   stored as `wine` in the settings.
3. **Wine prefix**: the Windows folder games run in (default `~/Games/xtux-prefix`), prepared with DXVK,
   vkd3d-proton, the Microsoft root certificates and the controller mode. The same as `xtux-cli prefix init`.
4. **Game library**: `~/.local/share/xtux/games` and `XboxGames` folders of Windows drives below `/mnt`, `/media`,
   `/run/media/$USER` and the home folder (for example `~/winGames/XboxGames`) are found by themselves; more folders
   can be added.
5. **Controllers**: PlayStation controllers reach games as Xbox gamepads (recommended) or as themselves.

**Set everything up** in the launcher runs all missing steps in this order; only the sign-in needs you.

## Terminal

`xtux-cli` without a command shows what is set up and a menu:

```
  ✓ Signed in as you@example.com
  ✓ Tux-Force Wine wine-20261118-1d9aa74
  ✓ Prefix /home/you/Games/xtux-prefix
  ✓ 8 games in 3 library folders
◆ What would you like to do?
› ▶  Play a game
  ↻  Check for updates
  ⬇  Downloads
  ⚙  Setup
  ✕  Quit
```

Arrow keys move, Enter chooses, typing filters lists, Esc goes back. Without a terminal (scripts, pipes) it prints
the overview only. All commands keep working as before; `xtux-cli --help` lists them.

## Flatpak

Releases include `xtux-linux-x86_64.flatpak`. It brings the GNOME runtime with WebKitGTK for the sign-in, so
nothing else needs to be installed:

```bash
flatpak install --user xtux-linux-x86_64.flatpak
flatpak run io.github.tux_force.xTux
```

The terminal client runs with `flatpak run --command=xtux-cli io.github.tux_force.xTux`. Inside the Flatpak,
settings and the downloaded Wine live below `~/.var/app/io.github.tux_force.xTux/`; the app can read the home
folder, `/mnt`, `/media` and `/run/media` for game libraries and uses the devices for the GPU and controllers.
The manifest is `packaging/flatpak/io.github.tux_force.xTux.yml`; the CI builds the bundle from its own release
binaries.
