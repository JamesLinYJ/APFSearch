//! Shared immutable filesystem identity for verified content reads and hashes.
//! Keeping both nanosecond timestamps catches same-size writes with restored mtime.
use serde::Serialize;
use std::{fs::Metadata, os::unix::fs::MetadataExt};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct FileIdentity {
    pub device_id: u64,
    pub file_id: u64,
    pub size: u64,
    pub modified: i64,
    pub modified_nsec: i64,
    pub changed: i64,
    pub changed_nsec: i64,
}
impl FileIdentity {
    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device_id: metadata.dev(),
            file_id: metadata.ino(),
            size: metadata.len(),
            modified: metadata.mtime(),
            modified_nsec: metadata.mtime_nsec(),
            changed: metadata.ctime(),
            changed_nsec: metadata.ctime_nsec(),
        }
    }
    pub(crate) fn modified_ns(&self) -> i64 {
        self.modified * 1_000_000_000 + self.modified_nsec
    }
    pub(crate) fn changed_ns(&self) -> i64 {
        self.changed * 1_000_000_000 + self.changed_nsec
    }
    pub(crate) fn matches_entry(&self, entry: &crate::index_store::IndexedFile) -> bool {
        self.file_id == entry.file_id
            && self.size == entry.size
            && self.modified_ns() == entry.modified_ns
            && self.changed_ns() == entry.changed_ns
    }
}
