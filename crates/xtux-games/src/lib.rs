//! Game level operations of xTux, shared by the daemon (`xtux-service`) and the clients (`xtux-cli`, the launcher).
//!
//! Nothing here prints or prompts: functions return [`Error`] and report progress through [`Progress`].

pub mod error;
pub mod license;
pub mod progress;

#[cfg(unix)]
pub mod bridge;
#[cfg(unix)]
pub mod link;
#[cfg(unix)]
pub mod prefix;

pub use error::{Error, Result};
pub use progress::{Progress, Silent};
