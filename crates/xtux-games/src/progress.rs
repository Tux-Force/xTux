/// Receives progress of long running operations. Clients show it; the daemon forwards it as events.
pub trait Progress: Send + Sync {
    /// A human readable step, for example "Downloading dxvk-3.1.1.tar.gz".
    fn message(&self, text: &str);

    /// Byte progress of the current step, `total` when known.
    fn bytes(&self, _done: u64, _total: Option<u64>) {}

    /// Byte progress of one file inside a step that handles several files at once (for example a
    /// streaming install). The first call for a name starts the file; [`Progress::bytes`] still
    /// reports the overall progress.
    fn file(&self, _name: &str, _done: u64, _total: u64) {}

    /// The file reported through [`Progress::file`] is complete.
    fn file_finished(&self, _name: &str) {}
}

/// Ignores all progress.
pub struct Silent;

impl Progress for Silent {
    fn message(&self, _text: &str) {}
}
