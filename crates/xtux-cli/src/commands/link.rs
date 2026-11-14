//! Use a game installed by the Windows Xbox app from Linux without copying any data.
//!
//! The Xbox app keeps a game as a `Content` folder next to a GUID named XVC package file
//! (with `.xvi`, `.xvs`, `.xct` and `.smd` companions). Xodus expects the content files directly
//! in the game folder and the package as `.xodus-streaming.msixvc`. A folder of symbolic links
//! bridges both layouts, so Windows and Linux can share the same installation.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const PACKAGE_LINK: &str = ".xodus-streaming.msixvc";

/// Whether the folder is a game installed by the Windows Xbox app.
pub fn is_windows_install(dir: &Path) -> bool {
    dir.join("Content").is_dir() && find_package(dir).is_some()
}

/// Finds the package file: a GUID named file that has an `.xvi` file next to it.
fn find_package(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let name = path.file_name()?.to_str()?.to_owned();
        let is_guid = name.len() == 36
            && name
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-');
        (is_guid && path.is_file() && dir.join(format!("{name}.xvi")).is_file()).then_some(path)
    })
}

/// Default place for link folders: `$XDG_DATA_HOME/xtux/links` or `~/.local/share/xtux/links`.
fn default_link_root() -> io::Result<PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data).join("xtux/links"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::other("HOME is not set, pass a destination explicitly"))?;
    Ok(PathBuf::from(home).join(".local/share/xtux/links"))
}

fn replace_link(target: &Path, link: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(link) {
        if !meta.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "{} exists and is not a symbolic link",
                link.display()
            )));
        }
        fs::remove_file(link)?;
    }
    symlink(target, link)
}

/// Creates the link folder for a Windows install and returns its path.
pub fn create_links(game_dir: &Path, dest: Option<&Path>) -> io::Result<PathBuf> {
    let game_dir = fs::canonicalize(game_dir)?;
    let package = find_package(&game_dir).ok_or_else(|| {
        io::Error::other("no XVC package found, is this a folder installed by the Xbox app?")
    })?;
    let content = game_dir.join("Content");
    if !content.is_dir() {
        return Err(io::Error::other("the folder has no Content directory"));
    }

    let dest = match dest {
        Some(dest) => dest.to_path_buf(),
        None => default_link_root()?.join(
            game_dir
                .file_name()
                .ok_or_else(|| io::Error::other("invalid game folder name"))?,
        ),
    };
    fs::create_dir_all(&dest)?;

    for entry in fs::read_dir(&content)? {
        let entry = entry?;
        replace_link(&entry.path(), &dest.join(entry.file_name()))?;
    }
    replace_link(&package, &dest.join(PACKAGE_LINK))?;

    fs::canonicalize(&dest)
}

/// Lets `run` accept a Windows Xbox app install directly.
pub fn resolve_source(source: &str) -> io::Result<String> {
    let path = Path::new(source);
    if path.join(PACKAGE_LINK).exists() || !is_windows_install(path) {
        return Ok(source.to_owned());
    }
    let links = create_links(path, None)?;
    eprintln!(
        "Using the Windows installation through {}",
        links.display()
    );
    Ok(links.to_string_lossy().into_owned())
}

pub fn run(game_dir: String, dest: Option<String>) -> ExitCode {
    match create_links(Path::new(&game_dir), dest.as_deref().map(Path::new)) {
        Ok(links) => {
            println!("{}", links.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
