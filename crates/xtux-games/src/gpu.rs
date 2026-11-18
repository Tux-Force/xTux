//! Graphics memory: how much is free and which programs hold it.
//!
//! NVIDIA through `nvidia-smi`, AMD through the amdgpu files in `/sys/class/drm` and the DRM `fdinfo` of the
//! user's own processes. Everything here is best effort and quick: a missing tool or file just means less is known.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Longest time `nvidia-smi` may take; the launch never waits longer than this for the check.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Programs below this are not named; the desktop itself takes a few hundred MiB.
const MIN_USER_MIB: u64 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuMemory {
    pub name: String,
    pub total_mib: u64,
    pub free_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VramUser {
    pub pid: u32,
    /// Short program name (`/proc/<pid>/comm`), never the command line.
    pub name: String,
    pub used_mib: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VramStatus {
    pub gpus: Vec<GpuMemory>,
    /// Largest first.
    pub users: Vec<VramUser>,
}

impl VramStatus {
    /// Programs that hold a noticeable part of the graphics memory, largest first, at most `max`.
    pub fn big_users(&self, max: usize) -> impl Iterator<Item = &VramUser> {
        self.users
            .iter()
            .filter(|u| u.used_mib >= MIN_USER_MIB)
            .take(max)
    }

    /// "ollama (19.8 GB), kwin_wayland (0.3 GB)"; empty when nobody holds much.
    pub fn users_text(&self, max: usize) -> String {
        self.big_users(max)
            .map(|u| format!("{} ({})", u.name, gb(u.used_mib)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A warning when the graphics card with the most memory has less than a quarter of it free.
    pub fn low_memory_warning(&self) -> Option<String> {
        let gpu = self.gpus.iter().max_by_key(|g| g.total_mib)?;
        if gpu.total_mib == 0 || gpu.free_mib * 4 >= gpu.total_mib {
            return None;
        }
        let mut text = format!(
            "Only {} of {} graphics memory is free.",
            gb(gpu.free_mib),
            gb(gpu.total_mib)
        );
        let users = self.users_text(3);
        if !users.is_empty() {
            text.push_str(&format!(" In use by: {users}."));
        }
        text.push_str(
            " Games can crash when it runs out. Close these programs before playing if you can.",
        );
        Some(text)
    }
}

/// "19.8 GB".
fn gb(mib: u64) -> String {
    format!("{:.1} GB", mib as f64 / 1024.0)
}

/// Free graphics memory and its users now. Empty when nothing can be read.
pub async fn status() -> VramStatus {
    let mut status = nvidia().await.unwrap_or_default();
    if status.gpus.is_empty() {
        status = tokio::task::spawn_blocking(amd).await.unwrap_or_default();
    }
    status.users.sort_by_key(|u| std::cmp::Reverse(u.used_mib));
    status
}

async fn nvidia_query(args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        QUERY_TIMEOUT,
        Command::new("nvidia-smi")
            .args(args)
            .arg("--format=csv,noheader,nounits")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn nvidia() -> Option<VramStatus> {
    let gpus =
        parse_nvidia_gpus(&nvidia_query(&["--query-gpu=name,memory.total,memory.free"]).await?);
    let apps = nvidia_query(&["--query-compute-apps=pid,process_name,used_memory"])
        .await
        .unwrap_or_default();
    let mut users = parse_nvidia_apps(&apps);
    for user in &mut users {
        if let Some(comm) = process_name(user.pid) {
            user.name = comm;
        }
    }
    Some(VramStatus { gpus, users })
}

/// Lines of `name, total, free` (MiB). The name is everything before the last two fields.
fn parse_nvidia_gpus(text: &str) -> Vec<GpuMemory> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.rsplitn(3, ',');
            let free_mib = fields.next()?.trim().parse().ok()?;
            let total_mib = fields.next()?.trim().parse().ok()?;
            let name = clean_name(fields.next()?);
            Some(GpuMemory {
                name,
                total_mib,
                free_mib,
            })
        })
        .collect()
}

/// Lines of `pid, process name, used` (MiB). The process name may hold commas (it can be a whole command line),
/// so the pid is the first field and the memory the last. One entry per pid, summed over the GPUs.
fn parse_nvidia_apps(text: &str) -> Vec<VramUser> {
    let mut by_pid: HashMap<u32, VramUser> = HashMap::new();
    for line in text.lines() {
        let Some((pid, rest)) = line.split_once(',') else {
            continue;
        };
        let Some((name, used)) = rest.rsplit_once(',') else {
            continue;
        };
        let (Ok(pid), Ok(used_mib)) = (pid.trim().parse::<u32>(), used.trim().parse::<u64>())
        else {
            continue;
        };
        // The program file without its arguments, as a fallback for `/proc/<pid>/comm`.
        let program = name.split_whitespace().next().unwrap_or_default();
        let program = program.rsplit('/').next().unwrap_or(program);
        by_pid
            .entry(pid)
            .and_modify(|u| u.used_mib += used_mib)
            .or_insert(VramUser {
                pid,
                name: clean_name(program),
                used_mib,
            });
    }
    by_pid.into_values().collect()
}

/// `/proc/<pid>/comm`, readable for every process.
fn process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name = clean_name(&comm);
    (!name.is_empty()).then_some(name)
}

/// Printable characters only, at most 40, so names are safe to show in one line.
fn clean_name(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect()
}

/// AMD cards with the amdgpu driver, and the user's own processes that hold their memory.
fn amd() -> VramStatus {
    let mut gpus = Vec::new();
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return VramStatus::default();
    };
    for card in cards.flatten() {
        let name = card.file_name().to_string_lossy().into_owned();
        // `card0`, not the connectors (`card0-DP-1`).
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device = card.path().join("device");
        let read = |file: &str| -> Option<u64> {
            std::fs::read_to_string(device.join(file))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let (Some(total), Some(used)) = (read("mem_info_vram_total"), read("mem_info_vram_used"))
        else {
            continue;
        };
        let name = std::fs::read_to_string(device.join("product_name"))
            .ok()
            .map(|n| clean_name(&n))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "AMD graphics card".into());
        gpus.push(GpuMemory {
            name,
            total_mib: total >> 20,
            free_mib: total.saturating_sub(used) >> 20,
        });
    }
    let users = if gpus.is_empty() {
        Vec::new()
    } else {
        drm_users(Path::new("/proc"))
    };
    VramStatus { gpus, users }
}

/// Processes with `drm-memory-vram` in the fdinfo of their DRM files. Only the user's own processes are readable,
/// which is what matters: other users' programs cannot be closed anyway.
fn drm_users(proc: &Path) -> Vec<VramUser> {
    let mut users = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc) else {
        return users;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fdinfo")) else {
            continue;
        };
        // One DRM client can be open through several fds; count each client once.
        let mut clients: HashMap<String, u64> = HashMap::new();
        for fd in fds.flatten() {
            let Ok(info) = std::fs::read_to_string(fd.path()) else {
                continue;
            };
            if let Some((client, kib)) = parse_drm_fdinfo(&info) {
                clients.insert(client, kib);
            }
        }
        let used_mib = clients.values().sum::<u64>() >> 10;
        if used_mib > 0 {
            users.push(VramUser {
                pid,
                name: process_name(pid).unwrap_or_else(|| pid.to_string()),
                used_mib,
            });
        }
    }
    users
}

/// `(drm-client-id, drm-memory-vram in KiB)` of one fdinfo file.
fn parse_drm_fdinfo(info: &str) -> Option<(String, u64)> {
    let mut client = None;
    let mut vram = None;
    for line in info.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "drm-client-id" => client = Some(value.trim().to_string()),
            "drm-memory-vram" => {
                let mut parts = value.split_whitespace();
                let amount: u64 = parts.next()?.parse().ok()?;
                vram = Some(match parts.next() {
                    Some("MiB") => amount << 10,
                    Some("GiB") => amount << 20,
                    Some("KiB") => amount,
                    _ => amount >> 10,
                });
            }
            _ => {}
        }
    }
    Some((client?, vram?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nvidia_gpus() {
        let gpus = parse_nvidia_gpus("NVIDIA GeForce RTX 3090, 24576, 21830\nbroken line\n");
        assert_eq!(
            gpus,
            vec![GpuMemory {
                name: "NVIDIA GeForce RTX 3090".into(),
                total_mib: 24576,
                free_mib: 21830
            }]
        );
    }

    #[test]
    fn reads_nvidia_apps_with_commas_in_the_name() {
        let mut users = parse_nvidia_apps(
            "3046, /usr/bin/kwin_wayland, 171\n\
             4076, /usr/share/app/app --type=gpu --x=1?2,3, 96\n\
             900, /usr/bin/ollama, 18000\n900, /usr/bin/ollama, 2000\n",
        );
        users.sort_by_key(|u| u.pid);
        assert_eq!(users.len(), 3);
        assert_eq!(users[0].name, "ollama");
        assert_eq!(users[0].used_mib, 20000);
        assert_eq!(users[2].name, "app");
        assert_eq!(users[2].used_mib, 96);
    }

    #[test]
    fn reads_drm_fdinfo() {
        let info = "pos:\t0\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\ndrm-memory-vram:\t2048 MiB\n";
        assert_eq!(parse_drm_fdinfo(info), Some(("42".into(), 2048 << 10)));
        assert_eq!(parse_drm_fdinfo("pos:\t0\n"), None);
    }

    fn status(free: u64) -> VramStatus {
        VramStatus {
            gpus: vec![GpuMemory {
                name: "Card".into(),
                total_mib: 24576,
                free_mib: free,
            }],
            users: vec![
                VramUser {
                    pid: 1,
                    name: "ollama".into(),
                    used_mib: 20275,
                },
                VramUser {
                    pid: 2,
                    name: "tiny".into(),
                    used_mib: 50,
                },
            ],
        }
    }

    #[test]
    fn warns_only_when_memory_is_low() {
        assert_eq!(status(20000).low_memory_warning(), None);
        let warning = status(2150).low_memory_warning().unwrap();
        assert!(warning.starts_with("Only 2.1 GB of 24.0 GB graphics memory is free."));
        assert!(warning.contains("In use by: ollama (19.8 GB)."));
        assert!(!warning.contains("tiny"));
        assert_eq!(VramStatus::default().low_memory_warning(), None);
    }
}
