//! Helpers to stop long running operations through a [`CancellationToken`].

use std::future::Future;

use tokio_util::sync::CancellationToken;

use crate::{Error, Result};

/// Runs `future` until it finishes or `cancel` fires, whichever comes first.
///
/// On cancellation the future is dropped at its next await point (between chunks or files)
/// and [`Error::Cancelled`] is returned.
pub(crate) async fn until_cancelled<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = future => result,
    }
}

/// Returns [`Error::Cancelled`] when `cancel` has fired.
pub(crate) fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
