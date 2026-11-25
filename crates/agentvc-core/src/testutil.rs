//! Test helpers.

use std::ops::Deref;

/// A temporary directory that unmounts any overlay views under it before it
/// is deleted (forks use overlayfs when the tests run as root).
pub struct TestDir(tempfile::TempDir);

pub fn tempdir() -> TestDir {
    TestDir(tempfile::tempdir().unwrap())
}

impl Deref for TestDir {
    type Target = tempfile::TempDir;
    fn deref(&self) -> &tempfile::TempDir {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        crate::overlay::unmount_under(self.0.path());
    }
}
