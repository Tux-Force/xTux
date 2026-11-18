//! Why a game ended abnormally, told in plain words.
//!
//! Reads the end of Wine's output (the game log of [`crate::logs::game_log`]) and, where it exists, the log the
//! game writes itself into its package folder of the Wine prefix
//! (`AppData/Local/Packages/<Identity Name>_<hash>/LocalState/log.txt`). Only fixed, known patterns count. The
//! texts handed to clients are fixed sentences plus at most a cleaned DLL or function name; no log content.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::gpu::VramStatus;

/// End of a log that is searched; crash messages are near the end.
const TAIL_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashKind {
    /// The Microsoft Store did not grant a license while the game ran.
    License,
    /// Graphics (or main) memory ran out.
    OutOfMemory,
    /// The graphics driver lost or reset the device.
    GpuLost,
    /// A DLL the game needs is missing.
    MissingDll,
    /// The game called a Windows function Wine does not have.
    Unimplemented,
    /// An error inside the game that it did not handle (page fault, exception).
    Exception,
    /// The process was ended by a signal.
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    pub kind: CrashKind,
    /// What happened, one sentence.
    pub reason: String,
    /// What the user can do, one or two sentences.
    pub hint: String,
}

/// What is known about the end of a game.
#[derive(Debug, Default)]
pub struct Ending<'a> {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Wine's output.
    pub wine_log: Option<&'a Path>,
    /// The game's own log, only when it was written during this run (see [`own_log`]).
    pub own_log: Option<&'a Path>,
    /// The game bridge failed to get a license token while the game ran.
    pub license_failed: bool,
    /// Graphics memory after the exit, to name programs that still hold it.
    pub vram: Option<&'a VramStatus>,
}

/// The crash, when the game ended abnormally and a known cause is found. A plain non-zero exit code without any
/// other sign is not reported: some games end that way normally.
pub fn classify(ending: &Ending) -> Option<Crash> {
    if ending.code == Some(0) {
        return None;
    }
    let wine = ending
        .wine_log
        .map(|p| crate::logs::tail(p, TAIL_BYTES))
        .unwrap_or_default();
    let own = ending
        .own_log
        .map(|p| crate::logs::tail(p, TAIL_BYTES))
        .unwrap_or_default();
    classify_text(ending, &wine, &own)
}

fn classify_text(ending: &Ending, wine: &str, own: &str) -> Option<Crash> {
    if ending.code == Some(0) {
        return None;
    }
    if ending.license_failed {
        return Some(crash(
            CrashKind::License,
            "The Microsoft Store did not grant a license for this game while it was running.",
            "Check that you are online and signed in with the account that owns the game or has Game Pass, then start the game again.",
        ));
    }

    const OWN_MEMORY: &[&str] = &[
        "Run out of memory",
        "Out of video memory",
        "out of video memory",
        "Failed to allocate",
        "E_OUTOFMEMORY",
    ];
    const WINE_GPU_MEMORY: &[&str] = &[
        "VK_ERROR_OUT_OF_DEVICE_MEMORY",
        "Memory allocation failed",
        "DXGI_ERROR_OUT_OF_MEMORY",
    ];
    if contains_any(own, OWN_MEMORY) || contains_any(wine, WINE_GPU_MEMORY) {
        let users = ending.vram.map(|v| v.users_text(3)).unwrap_or_default();
        let hint = if users.is_empty() {
            "Close other programs, especially ones that use the graphics card (for example local AI models or video tools), or lower the texture quality in the game's settings.".to_string()
        } else {
            format!(
                "These programs use a lot of graphics memory: {users}. Close them and start the game again, or lower the texture quality in the game's settings."
            )
        };
        return Some(Crash {
            kind: CrashKind::OutOfMemory,
            reason: "The game ran out of graphics memory.".into(),
            hint,
        });
    }

    const GPU_LOST: &[&str] = &[
        "VK_ERROR_DEVICE_LOST",
        "DXGI_ERROR_DEVICE_REMOVED",
        "DXGI_ERROR_DEVICE_HUNG",
        "DXGI_ERROR_DEVICE_RESET",
    ];
    if contains_any(wine, GPU_LOST) || contains_any(own, GPU_LOST) {
        return Some(crash(
            CrashKind::GpuLost,
            "The graphics driver stopped responding while the game was running.",
            "Update the graphics driver and close other programs that use the graphics card. If the card is overclocked, try its default clocks.",
        ));
    }

    if let Some(dll) = after(wine, "err:module:import_dll Library ", " (") {
        return Some(Crash {
            kind: CrashKind::MissingDll,
            reason: format!("The game needs {dll}, which is missing."),
            hint: "Set up the Wine prefix again (`xtux-cli prefix init`) so the graphics and runtime libraries are installed, then start the game again.".into(),
        });
    }

    if let Some(function) = after(wine, "to unimplemented function ", ",") {
        return Some(Crash {
            kind: CrashKind::Unimplemented,
            reason: format!("The game called {function}, which Wine does not have yet."),
            hint: "This is a gap in Wine, not a problem on your computer. Please report it with the log so it can be fixed.".into(),
        });
    }

    const EXCEPTION: &[&str] = &[
        "Unhandled page fault",
        "Unhandled exception",
        "Unhandled stack overflow",
        "Unhandled illegal instruction",
        "Unhandled privileged instruction",
        "winedbg: Internal crash",
        "starting debugger...",
    ];
    if contains_any(wine, EXCEPTION) {
        return Some(crash(
            CrashKind::Exception,
            "The game crashed with an error it could not handle.",
            "Start the game again. If it keeps crashing at the same point, close other programs (especially ones that use the graphics card) and check the log.",
        ));
    }

    match ending.signal {
        // SIGKILL: most often the kernel's out of memory killer.
        Some(9) => Some(crash(
            CrashKind::Killed,
            "The game was stopped by the system.",
            "This often happens when the computer runs out of memory. Close other programs and start the game again.",
        )),
        Some(signal) => Some(Crash {
            kind: CrashKind::Killed,
            reason: format!("Wine stopped unexpectedly (signal {signal})."),
            hint: "Start the game again. If it happens again, please report it with the log."
                .into(),
        }),
        None => None,
    }
}

fn crash(kind: CrashKind, reason: &str, hint: &str) -> Crash {
    Crash {
        kind,
        reason: reason.into(),
        hint: hint.into(),
    }
}

fn contains_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| text.contains(p))
}

/// The cleaned word between `start` and `end` on the last line that has both: letters, digits and `._-` only, at
/// most 64 characters. `None` when nothing usable is there.
fn after(text: &str, start: &str, end: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let rest = &line[line.find(start)? + start.len()..];
        let word = &rest[..rest.find(end)?];
        let clean: String = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
            .take(64)
            .collect();
        (!clean.is_empty()).then_some(clean)
    })
}

/// The game's own log in the prefix, when it was written at or after `since` (this run).
///
/// The folder is `<prefix>/drive_c/users/<user>/AppData/Local/Packages/<Identity Name>_<publisher hash>`; the
/// identity comes from the game's `MicrosoftGame.config`. Only `LocalState/log.txt` is looked at.
pub fn own_log(game_dir: &Path, prefix: &Path, since: SystemTime) -> Option<PathBuf> {
    let layout = crate::layout::detect(game_dir)?;
    let config = crate::library::read_config(&layout.root)?;
    let identity = crate::library::attribute(&config, "Identity", "Name")?;
    // A package name has no path characters; anything else is not used to build a path.
    if identity.is_empty()
        || !identity
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
    {
        return None;
    }
    let prefix_name = format!("{identity}_");
    let users = std::fs::read_dir(prefix.join("drive_c/users")).ok()?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for user in users.flatten() {
        let Ok(packages) = std::fs::read_dir(user.path().join("AppData/Local/Packages")) else {
            continue;
        };
        for package in packages.flatten() {
            if !package
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix_name)
            {
                continue;
            }
            let log = package.path().join("LocalState/log.txt");
            // A regular file, not a link somewhere else.
            let Ok(meta) = std::fs::symlink_metadata(&log) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if meta.is_file()
                && modified >= since
                && newest.as_ref().is_none_or(|(time, _)| modified > *time)
            {
                newest = Some((modified, log));
            }
        }
    }
    newest.map(|(_, log)| log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{GpuMemory, VramUser};

    fn ending() -> Ending<'static> {
        Ending {
            code: Some(84),
            ..Default::default()
        }
    }

    const PAGE_FAULT: &str = "0024:fixme:dxgi:foo Unhandled flags 0x800.\n\
        wine: Unhandled page fault on read access to 0000000000000000 at address 0000000140001000 (thread 0024), starting debugger...\n\
        0024:err:seh:start_debugger Couldn't start debugger L\"winedbg --auto 32 72\"\n\
        winedbg: Internal crash at 00006FFFFFC4D3A1\n";

    #[test]
    fn clean_exit_is_no_crash() {
        let e = Ending {
            code: Some(0),
            license_failed: true,
            ..Default::default()
        };
        assert_eq!(classify_text(&e, PAGE_FAULT, ""), None);
    }

    #[test]
    fn plain_exit_code_is_no_crash() {
        assert_eq!(
            classify_text(&ending(), "fixme: Unhandled flags 0x800\n", ""),
            None
        );
    }

    #[test]
    fn page_fault_is_an_exception() {
        let crash = classify_text(&ending(), PAGE_FAULT, "").unwrap();
        assert_eq!(crash.kind, CrashKind::Exception);
    }

    #[test]
    fn own_log_memory_error_wins_over_the_page_fault() {
        let own = "  Error: Failed to read button mapping in file C:/x.xml\n\
                   Error: Run out of memory\nError: Failed to allocate ImageResource\n";
        let vram = VramStatus {
            gpus: vec![GpuMemory {
                name: "Card".into(),
                total_mib: 24576,
                free_mib: 2000,
            }],
            users: vec![VramUser {
                pid: 7,
                name: "ollama".into(),
                used_mib: 20275,
            }],
        };
        let e = Ending {
            vram: Some(&vram),
            ..ending()
        };
        let crash = classify_text(&e, PAGE_FAULT, own).unwrap();
        assert_eq!(crash.kind, CrashKind::OutOfMemory);
        assert!(crash.hint.contains("ollama (19.8 GB)"), "{}", crash.hint);
    }

    #[test]
    fn harmless_own_log_errors_do_not_count() {
        let own = "Error: Failed to read button mapping in file C:/x.xml\nXbox generic error: (Failed to register for invites\n";
        assert_eq!(classify_text(&ending(), "", own), None);
    }

    #[test]
    fn device_lost() {
        let wine = "err: D3D12: vkQueueSubmit failed: VK_ERROR_DEVICE_LOST\n";
        assert_eq!(
            classify_text(&ending(), wine, "").unwrap().kind,
            CrashKind::GpuLost
        );
    }

    #[test]
    fn names_the_missing_dll() {
        let wine = "0024:err:module:import_dll Library XInput1_4.dll (which is needed by L\"C:\\\\game\\\\a.exe\") not found\n";
        let crash = classify_text(&ending(), wine, "").unwrap();
        assert_eq!(crash.kind, CrashKind::MissingDll);
        assert_eq!(
            crash.reason,
            "The game needs XInput1_4.dll, which is missing."
        );
    }

    #[test]
    fn names_the_unimplemented_function_cleaned() {
        let wine = "wine: Call from 00006FFFFFC4D3A1 to unimplemented function api-ms-win-core.dll.Foo\u{1b}[0m<b>, aborting\n";
        let crash = classify_text(&ending(), wine, "").unwrap();
        assert_eq!(crash.kind, CrashKind::Unimplemented);
        assert!(
            crash.reason.contains("api-ms-win-core.dll.Foo0mb"),
            "{}",
            crash.reason
        );
    }

    #[test]
    fn license_failure_comes_first() {
        let e = Ending {
            license_failed: true,
            ..ending()
        };
        assert_eq!(
            classify_text(&e, PAGE_FAULT, "").unwrap().kind,
            CrashKind::License
        );
    }

    #[test]
    fn signals() {
        let killed = Ending {
            code: None,
            signal: Some(9),
            ..Default::default()
        };
        assert_eq!(
            classify_text(&killed, "", "").unwrap().kind,
            CrashKind::Killed
        );
        let segv = Ending {
            code: None,
            signal: Some(11),
            ..Default::default()
        };
        assert!(
            classify_text(&segv, "", "")
                .unwrap()
                .reason
                .contains("signal 11")
        );
    }

    #[test]
    fn finds_the_own_log_of_this_run_only() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join(crate::layout::XODUS_PACKAGE), b"").unwrap();
        std::fs::write(
            game.join("MicrosoftGame.config"),
            "<Game><Identity\n  Name=\"Studio.Game\"\n  Publisher=\"CN=X\" /></Game>",
        )
        .unwrap();
        let prefix = dir.path().join("prefix");
        let state = prefix
            .join("drive_c/users/steamuser/AppData/Local/Packages/Studio.Game_abc123/LocalState");
        std::fs::create_dir_all(&state).unwrap();
        let before = SystemTime::now() - std::time::Duration::from_secs(60);
        assert_eq!(own_log(&game, &prefix, before), None);
        std::fs::write(state.join("log.txt"), "hello").unwrap();
        assert_eq!(own_log(&game, &prefix, before), Some(state.join("log.txt")));
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(own_log(&game, &prefix, later), None);
    }
}
