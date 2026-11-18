//! Gamepads for moving through the launcher: read from the kernel's event devices (evdev), so Xbox and PlayStation
//! controllers work over USB and Bluetooth without SDL. Controllers plugged in later are picked up.
//!
//! The thread turns buttons and sticks into actions (`up`, `down`, `left`, `right`, `a`, `b`, `x`, `y`, `lb`, `rb`,
//! `start`), with key repeat while a direction is held, and says which kind of pad sent them (for the button icons). Whoever receives them decides whether they apply (the
//! launcher ignores them while it is not focused or a game runs).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode};

const POLL: Duration = Duration::from_millis(16);
const RESCAN: Duration = Duration::from_secs(2);
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_EVERY: Duration = Duration::from_millis(110);
/// Share of the stick's range from the centre that counts as a push.
const STICK_THRESHOLD: f32 = 0.6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    fn action(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// The action of a button; `None` for buttons the launcher does not use.
fn button(code: KeyCode) -> Option<&'static str> {
    Some(match code {
        // Cross on PlayStation controllers.
        KeyCode::BTN_SOUTH => "a",
        // Circle.
        KeyCode::BTN_EAST => "b",
        // Square (BTN_WEST) and Triangle (BTN_NORTH).
        KeyCode::BTN_WEST => "x",
        KeyCode::BTN_NORTH => "y",
        KeyCode::BTN_TL => "lb",
        KeyCode::BTN_TR => "rb",
        // Options / Menu.
        KeyCode::BTN_START => "start",
        KeyCode::BTN_DPAD_UP => "up",
        KeyCode::BTN_DPAD_DOWN => "down",
        KeyCode::BTN_DPAD_LEFT => "left",
        KeyCode::BTN_DPAD_RIGHT => "right",
        _ => return None,
    })
}

/// Range of a stick axis.
#[derive(Debug, Clone, Copy)]
struct Range {
    min: i32,
    max: i32,
}

impl Range {
    /// `value` as -1.0 to 1.0.
    fn normalize(self, value: i32) -> f32 {
        let span = (self.max - self.min).max(1) as f32;
        (value - self.min) as f32 / span * 2.0 - 1.0
    }
}

struct Pad {
    device: Device,
    /// `playstation` (Sony) or `xbox` (everything else uses its button names).
    kind: &'static str,
    ranges: HashMap<AbsoluteAxisCode, Range>,
    /// Hat (D-pad as axes) and left stick, as -1, 0 or 1 per axis.
    hat: (i32, i32),
    stick: (i32, i32),
    /// D-pad buttons held (controllers that report the D-pad as buttons).
    dpad: Option<Direction>,
}

impl Pad {
    fn open(path: &PathBuf) -> Option<Self> {
        let device = Device::open(path).ok()?;
        // A gamepad has the south face button; keyboards and mice do not.
        if !device
            .supported_keys()
            .is_some_and(|keys| keys.contains(KeyCode::BTN_SOUTH))
        {
            return None;
        }
        device.set_nonblocking(true).ok()?;
        let ranges = device
            .get_absinfo()
            .map(|axes| {
                axes.map(|(axis, info)| {
                    (
                        axis,
                        Range {
                            min: info.minimum(),
                            max: info.maximum(),
                        },
                    )
                })
                .collect()
            })
            .unwrap_or_default();
        tracing::info!(
            "gamepad for the launcher: {}",
            device.name().unwrap_or("unnamed")
        );
        let kind = if device.input_id().vendor() == 0x054c {
            "playstation"
        } else {
            "xbox"
        };
        Some(Pad {
            device,
            kind,
            ranges,
            hat: (0, 0),
            stick: (0, 0),
            dpad: None,
        })
    }

    /// The direction held, the D-pad before the stick.
    fn held(&self) -> Option<Direction> {
        let pick = |(x, y): (i32, i32)| match (x, y) {
            (_, -1) => Some(Direction::Up),
            (_, 1) => Some(Direction::Down),
            (-1, _) => Some(Direction::Left),
            (1, _) => Some(Direction::Right),
            _ => None,
        };
        self.dpad
            .or_else(|| pick(self.hat))
            .or_else(|| pick(self.stick))
    }

    /// Reads what happened since the last call; button actions go to `send`. `Err` when the device is gone.
    fn read(&mut self, send: &dyn Fn(&'static str, &'static str)) -> std::io::Result<()> {
        let events: Vec<EventSummary> = match self.device.fetch_events() {
            Ok(events) => events.map(|e| e.destructure()).collect(),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err),
        };
        for event in events {
            match event {
                EventSummary::Key(_, code, value) => {
                    let direction = match code {
                        KeyCode::BTN_DPAD_UP => Some(Direction::Up),
                        KeyCode::BTN_DPAD_DOWN => Some(Direction::Down),
                        KeyCode::BTN_DPAD_LEFT => Some(Direction::Left),
                        KeyCode::BTN_DPAD_RIGHT => Some(Direction::Right),
                        _ => None,
                    };
                    match (direction, value) {
                        // Directions are sent by the repeat logic.
                        (Some(d), 1) => self.dpad = Some(d),
                        (Some(d), 0) if self.dpad == Some(d) => self.dpad = None,
                        (Some(_), _) => {}
                        (None, 1) => {
                            if let Some(action) = button(code) {
                                send(action, self.kind);
                            }
                        }
                        (None, _) => {}
                    }
                }
                EventSummary::AbsoluteAxis(_, axis, value) => {
                    let step = |range: Option<&Range>| {
                        let v = range.map_or(0.0, |r| r.normalize(value));
                        if v <= -STICK_THRESHOLD {
                            -1
                        } else if v >= STICK_THRESHOLD {
                            1
                        } else {
                            0
                        }
                    };
                    match axis {
                        AbsoluteAxisCode::ABS_HAT0X => self.hat.0 = value.signum(),
                        AbsoluteAxisCode::ABS_HAT0Y => self.hat.1 = value.signum(),
                        AbsoluteAxisCode::ABS_X => self.stick.0 = step(self.ranges.get(&axis)),
                        AbsoluteAxisCode::ABS_Y => self.stick.1 = step(self.ranges.get(&axis)),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Starts the reader thread; `send` is called on that thread for every action.
pub fn spawn(send: impl Fn(&'static str, &'static str) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("gamepad".into())
        .spawn(move || run(&send));
    if let Err(err) = spawned {
        tracing::warn!("gamepad support not started: {err}");
    }
}

fn run(send: &dyn Fn(&'static str, &'static str)) {
    let mut pads: HashMap<PathBuf, Pad> = HashMap::new();
    // Event devices that are no gamepad (or not readable), so they are not opened every scan.
    let mut skipped: Vec<PathBuf> = Vec::new();
    let mut last_scan: Option<Instant> = None;
    // The direction held and when it is sent next.
    let mut repeat: Option<(Direction, Instant)> = None;
    loop {
        if last_scan.is_none_or(|t| t.elapsed() >= RESCAN) {
            last_scan = Some(Instant::now());
            if let Ok(entries) = std::fs::read_dir("/dev/input") {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let event_node = path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("event"));
                    if !event_node || pads.contains_key(&path) || skipped.contains(&path) {
                        continue;
                    }
                    match Pad::open(&path) {
                        Some(pad) => {
                            pads.insert(path, pad);
                        }
                        None => skipped.push(path),
                    }
                }
            }
            // Nodes that went away may come back as another device.
            skipped.retain(|p| p.exists());
        }

        pads.retain(|path, pad| match pad.read(send) {
            Ok(()) => true,
            Err(err) => {
                tracing::info!("gamepad {} gone: {err}", path.display());
                false
            }
        });

        let held = pads.values().find_map(|pad| Some((pad.held()?, pad.kind)));
        let now = Instant::now();
        repeat = match (held, repeat) {
            (None, _) => None,
            (Some((d, kind)), Some((was, next))) if d == was => {
                if now >= next {
                    send(d.action(), kind);
                    Some((d, now + REPEAT_EVERY))
                } else {
                    Some((d, next))
                }
            }
            (Some((d, kind)), _) => {
                send(d.action(), kind);
                Some((d, now + REPEAT_DELAY))
            }
        };
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_range_is_normalized() {
        let range = Range { min: 0, max: 255 };
        assert!(range.normalize(0) <= -0.99);
        assert!(range.normalize(255) >= 0.99);
        assert!(range.normalize(128).abs() < 0.01);
    }

    #[test]
    fn face_buttons() {
        assert_eq!(button(KeyCode::BTN_SOUTH), Some("a"));
        assert_eq!(button(KeyCode::BTN_EAST), Some("b"));
        assert_eq!(button(KeyCode::KEY_A), None);
    }
}
