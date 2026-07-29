//! Scratch directories for tests.
//!
//! Shared by the `session` and `git` test modules, which each need a directory per test.
//! It lives here rather than being copied into both because the two copies had already
//! drifted: only one of them cleared, and the reason for clearing was written down in
//! neither -- which is how the surviving copy comes to look redundant and gets deleted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// A fresh directory under `%TEMP%\<root>`, removed when the returned guard drops.
///
/// It is cleared before it is created *and* removed after, and the two cover different
/// failures. The name is unique only per (pid, counter) and Windows recycles process ids,
/// so without the clear a run that inherits an aborted run's directory reads that run's
/// `sessions.json` and lock files and asserts against state it did not create. Verified
/// 2026-07-28: an aborted run left ~239 directories behind, the next run drew a matching
/// pid, and five session tests failed that passed again under a fresh pid.
///
/// Neither half is redundant. `Drop` does not run when the harness aborts, which is the
/// case above; the clear does not keep a passing run from leaving anything behind, which
/// is how those directories accumulated in the first place.
///
/// Clearing *this* directory rather than sweeping the shared parent is also deliberate:
/// two live runs cannot share a pid, so a per-pid clear can never delete a concurrent test
/// process's state, while a sweep of the parent could.
pub fn temp_dir(root: &str) -> TempDir {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir()
        .join(root)
        .join(format!("{}-{}", std::process::id(), n));
    match std::fs::remove_dir_all(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // A clear that fails leaves inherited state in place -- the exact failure this
        // guards against -- so say so here, rather than leaving it to surface as an
        // unexplained assertion failure further down the test.
        Err(e) => panic!("clear temp dir {}: {e}", path.display()),
    }
    std::fs::create_dir_all(&path).expect("create temp dir");
    TempDir { path }
}

/// Owns one test's directory and removes it when the test ends.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A failing test keeps its directory, because it is the evidence. That is safe to
        // do here and nowhere else: the clear in `temp_dir` means a retained directory can
        // never be read by a later run.
        if std::thread::panicking() {
            eprintln!("kept {} for inspection", self.path.display());
            return;
        }
        // Best effort, unlike the clear. A failure here cannot corrupt anything -- the
        // next user of this path clears it first -- and panicking in a drop would replace
        // the result of a test that had already passed.
        std::fs::remove_dir_all(&self.path).ok();
    }
}
