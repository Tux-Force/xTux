//! Game level operations of xTux, shared by the daemon (`xtux-service`) and the clients (`xtux-cli`, the launcher).
//!
//! Nothing here prints or prompts: functions return [`Error`] and report progress through [`Progress`].

pub mod art;
mod cancel;
pub mod download;
pub mod error;
pub mod install;
pub mod layout;
pub mod library;
pub mod license;
pub mod logs;
pub mod package;
pub mod progress;
pub mod settings;

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
