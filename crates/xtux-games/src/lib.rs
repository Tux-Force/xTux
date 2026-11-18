//! Game level operations of xTux, shared by the daemon (`xtux-service`) and the clients (`xtux-cli`, the launcher).
//!
//! Nothing here prints or prompts: functions return [`Error`] and report progress through [`Progress`].

mod access;
pub mod anticheat;
pub mod art;
mod cancel;
pub mod catalog;
pub mod crash;
pub mod download;
pub mod error;
#[cfg(unix)]
pub mod gameinput;
pub mod gpu;
pub mod install;
pub mod layout;
pub mod library;
pub mod license;
pub mod logs;
pub mod package;
pub mod packages;
pub mod progress;
pub mod queue;
pub mod settings;
pub mod uninstall;
pub mod updates;

#[cfg(unix)]
pub mod bridge;
#[cfg(unix)]
pub mod link;
#[cfg(unix)]
pub mod prefix;
#[cfg(unix)]
pub mod run;

pub use error::{Error, Result};
pub use progress::{Progress, Silent};
pub use tokio_util::sync::CancellationToken;
