//! The Tux-Force pixel mascot as terminal art, shown when a long running command starts.
//!
//! Drawn with half block characters in true colour, so every mascot pixel is square. Skipped when stdout is not a
//! terminal, when `NO_COLOR` is set or with `XTUX_NO_BANNER=1`.

use std::io::{IsTerminal, Write};
use std::time::Duration;

/// 16 x 19 pixels, from `brand/mascot-pixel.svg`.
const MASCOT: [&str; 19] = [
    "......####......",
    "....########....",
    "...##########...",
    "..###......###..",
    "..##........##..",
    "..##.##..##.##..",
    "..##.##..##.##..",
    "..##........##..",
    "..##..####..##..",
    "..###..##..###..",
    ".#####....#####.",
    ".###........###.",
    "###..........###",
    "##............##",
    "##............##",
    "###..........###",
    ".###........###.",
    "..############..",
    ".#####....#####.",
];

const LIME: (u8, u8, u8) = (0xA3, 0xE6, 0x35);
const EMERALD: (u8, u8, u8) = (0x10, 0xB9, 0x81);
const MIST: (u8, u8, u8) = (0x8F, 0xB5, 0xA3);
const RESET: &str = "\x1b[0m";

fn fg((r, g, b): (u8, u8, u8)) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// The mascot pixels, with the eyes closed when `blink` is set.
fn pixels(blink: bool) -> Vec<Vec<bool>> {
    MASCOT
        .iter()
        .enumerate()
        .map(|(row, line)| {
            line.chars()
                .enumerate()
                .map(|(col, c)| {
                    let eye = (row == 5 || row == 6) && matches!(col, 5 | 6 | 9 | 10);
                    // Closed eyes: a single dark line where the open eyes were two pixels high.
                    if blink && eye {
                        return row != 6;
                    }
                    c == '#'
                })
                .collect()
        })
        .collect()
}

/// Terminal lines of the mascot with the wordmark next to it.
fn frame(blink: bool) -> Vec<String> {
    let px = pixels(blink);
    let colour = fg(LIME);
    let text = [
        String::new(),
        String::new(),
        String::new(),
        format!(
            "\x1b[1m{}xTux{RESET}  {}by Tux-Force{RESET}",
            fg(LIME),
            fg(EMERALD)
        ),
        format!(
            "{}The PC games you own or subscribe to, on Linux.{RESET}",
            fg(MIST)
        ),
        format!("{}v{}{RESET}", fg(MIST), env!("CARGO_PKG_VERSION")),
    ];
    (0..px.len().div_ceil(2))
        .map(|line| {
            let top = &px[line * 2];
            let bottom = px.get(line * 2 + 1);
            let art: String = (0..top.len())
                .map(|col| match (top[col], bottom.is_some_and(|b| b[col])) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            let words = text.get(line).cloned().unwrap_or_default();
            format!("  {colour}{art}{RESET}   {words}")
        })
        .collect()
}

fn enabled() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && !std::env::var("XTUX_NO_BANNER").is_ok_and(|v| v == "1")
}

/// Draws the mascot line by line and lets it blink once.
pub fn show() {
    if !enabled() {
        return;
    }
    let mut out = std::io::stdout().lock();
    let open = frame(false);
    let _ = writeln!(out);
    for line in &open {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(35));
    }
    let redraw = |out: &mut std::io::StdoutLock, lines: &[String]| {
        let _ = write!(out, "\x1b[{}A", lines.len());
        for line in lines {
            let _ = writeln!(out, "\r\x1b[2K{line}");
        }
        let _ = out.flush();
    };
    std::thread::sleep(Duration::from_millis(250));
    redraw(&mut out, &frame(true));
    std::thread::sleep(Duration::from_millis(140));
    redraw(&mut out, &open);
    let _ = writeln!(out);
}

#[cfg(test)]
mod tests {
    #[test]
    fn mascot_is_rectangular() {
        assert!(super::MASCOT.iter().all(|row| row.len() == 16));
        assert_eq!(super::frame(false).len(), 10);
        assert_eq!(super::frame(true).len(), 10);
    }
}
