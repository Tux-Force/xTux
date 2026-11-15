// Hardware probing utilities

use std::io;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

use base64::prelude::*;
#[cfg(any(target_os = "macos", target_os = "ios", target_family = "windows"))]
use smbioslib::raw_smbios_from_device;
#[cfg(not(target_os = "linux"))]
use smbioslib::{SMBiosSystemInformation, SystemUuidData, table_load_from_device};

use crate::clep;
use crate::models::devicecredential::Component;

pub fn probe_provision_components() -> Vec<Component> {
    let mut components = Vec::with_capacity(16);
    let drive_serial = BASE64_STANDARD.decode("AA==").unwrap();
    let mut smbios_buf = [0; 256];
    let mut drive_buf = [0; 64];

    let smbios = load_raw_smbios().ok();
    let parsed_smbios = load_smbios_fields(smbios.as_deref()).ok();

    drive_buf
        .iter_mut()
        .zip(drive_serial.iter())
        .for_each(|(place, data)| *place = *data);
    if let Some(smbios) = smbios.as_ref() {
        smbios_buf
            .iter_mut()
            .zip(smbios.iter())
            .for_each(|(place, data)| *place = *data);
    }
    let (clepv2, clepv4) = clep::challenge::get_license_challange(smbios_buf, drive_buf);

    components.push(Component::new(4113, "AA==".to_string()));
    components.push(Component::error(4101));
    components.push(Component::new(8196, BASE64_STANDARD.encode(clepv2)));
    components.push(Component::new(8197, BASE64_STANDARD.encode(clepv4)));

    if let Some((version, serial, uuid)) = parsed_smbios {
        components.push(Component::new(4100, BASE64_STANDARD.encode(version)));
        components.push(Component::new(4101, BASE64_STANDARD.encode(serial)));
        components.push(Component::new(4102, BASE64_STANDARD.encode(uuid)));
    } else {
        components.push(Component::error(4100));
        components.push(Component::error(4101));
        components.push(Component::error(4102));
    }

    components.push(Component::new(4145, "AQAAAA==".to_string()));
    components.push(Component::error(4160));
    components.push(Component::error(4161));

    // Common values sent with the request
    // "4128"
    // "4130"
    // "4112"
    // "4113"
    // "4098"
    // "4099"
    // "4100"
    // "4101"
    // "4102"
    // "4097"
    // "8195"
    // "8196"
    // "8197"
    // "4144"
    // "4145"
    // "4160"
    // "4161"

    components
}

#[cfg(target_os = "linux")]
fn load_smbios_fields(raw: Option<&[u8]>) -> io::Result<(Vec<u8>, Vec<u8>, [u8; 16])> {
    let smbios =
        raw.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing raw SMBIOS data"))?;
    Ok(parse_smbios(smbios))
}

#[cfg(not(target_os = "linux"))]
fn load_smbios_fields(_raw: Option<&[u8]>) -> io::Result<(Vec<u8>, Vec<u8>, [u8; 16])> {
    let data = table_load_from_device()?;
    let system_info = data
        .first::<SMBiosSystemInformation>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing SMBIOS Type 1"))?;

    let version = system_info
        .version()
        .to_utf8_lossy()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing SMBIOS version"))?
        .into_bytes();
    let serial = system_info
        .serial_number()
        .to_utf8_lossy()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing SMBIOS serial"))?
        .into_bytes();
    let uuid = match system_info.uuid() {
        Some(SystemUuidData::Uuid(uuid)) => uuid.raw,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing SMBIOS UUID",
            ));
        }
    };

    Ok((version, serial, uuid))
}

#[cfg(target_os = "linux")]
fn parse_smbios(smbios: &[u8]) -> (Vec<u8>, Vec<u8>, [u8; 16]) {
    let length = smbios[1];

    let version = smbios[6];
    let serial = smbios[7];
    let uuid: [u8; 16] = smbios[8..24].try_into().unwrap();

    let stringsbuf = &smbios[length as usize..];
    let mut strings: Vec<&[u8]> = Vec::new();
    strings.push(&[]);
    let mut cursor = 0;
    while cursor < stringsbuf.len() {
        let end = stringsbuf[cursor..]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(stringsbuf.len() - cursor)
            + cursor;
        let slice = &stringsbuf[cursor..end];
        strings.push(slice);
        cursor = end + 1;
        if cursor >= stringsbuf.len() || stringsbuf[cursor] == 0 {
            break;
        }
    }

    (
        strings[version as usize].to_vec(),
        strings[serial as usize].to_vec(),
        uuid,
    )
}

#[cfg(any(target_os = "macos", target_os = "ios", target_family = "windows"))]
fn load_raw_smbios() -> io::Result<Vec<u8>> {
    raw_smbios_from_device()
}

/// Why the next password prompt appears. The polkit dialog only names the command, so xTux says it first: on the
/// terminal and in a dialog of its own.
#[cfg(target_os = "linux")]
const SMBIOS_PROMPT: &str = "xTux registers this PC with Microsoft once, like Windows does when it first signs in. \
     For that it reads the system information of the firmware (manufacturer, model, serial number, UUID from \
     /sys/firmware/dmi), which only root may read, so the system asks for your password. Cancelling is fine: the \
     registration then goes ahead without these values. XTUX_SKIP_SMBIOS=1 skips the question.";

#[cfg(target_os = "linux")]
const SMBIOS_DIALOG: &str = "xTux registers this PC with Microsoft once, like Windows does when it first signs in.\n\n\
     For that it reads the system information of the firmware (manufacturer, model, serial number, UUID). Only root \
     may read it, so the system will ask for your password next.\n\n\
     Skip registers the PC without these values; that works too.";

/// Asks in a desktop dialog whether to read the firmware information: `Some(true)` continue, `Some(false)` skip,
/// `None` when no dialog could be shown.
#[cfg(target_os = "linux")]
fn ask_in_dialog() -> Option<bool> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return None;
    }
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_uppercase().contains("KDE"));
    let kdialog = || {
        Command::new("kdialog")
            .args([
                "--title",
                "xTux",
                "--yes-label",
                "Continue",
                "--no-label",
                "Skip",
                "--yesno",
                SMBIOS_DIALOG,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    let zenity = || {
        Command::new("zenity")
            .args([
                "--question",
                "--title=xTux",
                "--ok-label=Continue",
                "--cancel-label=Skip",
                "--width=460",
                &format!("--text={SMBIOS_DIALOG}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    let tools: [&dyn Fn() -> io::Result<std::process::ExitStatus>; 2] = if kde {
        [&kdialog, &zenity]
    } else {
        [&zenity, &kdialog]
    };
    for tool in tools {
        match tool().ok().and_then(|status| status.code()) {
            Some(0) => return Some(true),
            Some(1) => return Some(false),
            _ => continue,
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn load_raw_smbios() -> io::Result<Vec<u8>> {
    if std::env::var("XTUX_SKIP_SMBIOS").is_ok_and(|v| v == "1") {
        return Err(io::Error::other("skipped by XTUX_SKIP_SMBIOS"));
    }
    tracing::warn!("{SMBIOS_PROMPT}");
    match ask_in_dialog() {
        Some(false) => {
            tracing::warn!("firmware system information skipped, registering the PC without it");
            return Err(io::Error::other("skipped in the dialog"));
        }
        Some(true) => {}
        // No dialog tool: at least a notification for sessions without a terminal (xtux-service).
        None => {
            let _ = Command::new("notify-send")
                .args([
                    "--app-name=xTux",
                    "xTux asks for your password",
                    SMBIOS_PROMPT,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let cmd = Command::new("/usr/bin/pkexec")
        .args(["cat", "/sys/firmware/dmi/entries/1-0/raw"])
        .stdout(Stdio::piped())
        .spawn()?;
    let output = cmd.wait_with_output()?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        tracing::warn!("no firmware system information, registering the PC without it");
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unable to probe SMBIOS data",
        ))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_family = "windows"
)))]
fn load_raw_smbios() -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "raw SMBIOS loading is unsupported on this platform",
    ))
}

/// Shows the dialog shown before the polkit prompt: `cargo test -p xodus smbios_dialog -- --ignored --nocapture`.
#[cfg(target_os = "linux")]
#[ignore]
#[test]
fn smbios_dialog() {
    println!("dialog answer: {:?}", ask_in_dialog());
}
