//! Windows metadata-only access. Unknown reparses never authorize traversal or deletion.

#[cfg(windows)]
pub(crate) mod native;
#[cfg(windows)]
pub use native::{NativeNames, RootHandle};

/// Stable identity from FILE_ID_INFO, scoped to its volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Volume serial returned by the open handle, not a drive-letter guess.
    pub volume: u64,
    /// NTFS 128-bit file identifier.
    pub file: [u8; 16],
}

/// Handle-observed metadata; obtaining it never reads a data stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryMetadata {
    /// Physical identity, distinct from the protocol's EntryId.
    pub identity: FileIdentity,
    /// Logical size, including unhydrated content.
    pub size: u64,
    /// Allocated bytes reported by the filesystem.
    pub allocated: u64,
    /// True for directories.
    pub directory: bool,
    /// Raw Windows file attributes for residency/change classification.
    pub attributes: u32,
    /// Reparse tag (zero for ordinary files).
    pub reparse_tag: u32,
    /// Hard-link count; multiple links are a user-visible problem.
    pub links: u32,
    /// Creation time in Windows ticks. Never used to decide the winning content.
    pub created: i64,
    /// Last-write time in Windows ticks. Never used as commit evidence.
    pub modified: i64,
    /// CFAPI metadata queried through the same handle, even when reparse attributes are hidden.
    pub cloud: Option<CloudMetadata>,
}

/// Metadata from CfGetPlaceholderInfo, independent of stale or hidden attribute bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudMetadata {
    /// Local content bytes; querying does not recall the file.
    pub on_disk: u64,
    /// Locally modified content bytes.
    pub modified: u64,
    /// CF_PIN_STATE, a user's preservation intent.
    pub pin_state: u32,
    /// CF_IN_SYNC_STATE; not a substitute for the business baseline.
    pub in_sync_state: u32,
    /// Provider identity, bounded by the CFAPI limit (4 KiB).
    pub identity: Vec<u8>,
}

impl EntryMetadata {
    /// Cloud tags include CLOUD, CLOUD_1, ... CLOUD_F. Other reparses are unsupported.
    #[must_use]
    pub fn placeholder(&self) -> bool {
        self.cloud.is_some() || self.reparse_tag & 0xffff_0fff == 0x9000_001a
    }

    /// True only for ordinary entries or documented Cloud Files reparses.
    #[must_use]
    pub fn supported(&self) -> bool {
        self.reparse_tag == 0 || self.reparse_tag & 0xffff_0fff == 0x9000_001a
    }
}

/// A scan failure leaves previous entries and baselines intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanHealth {
    /// Enumeration completed under the same pinned root.
    Complete,
    /// An ACL denied enumeration or metadata access.
    PermissionDenied,
    /// Root or volume identity changed or vanished.
    RootUnavailable,
    /// Enumeration was interrupted or an unsupported object was encountered.
    Incomplete,
}

/// A named entry or problem, preserving original spelling.
#[derive(Clone, Debug)]
pub struct ScanEntry {
    /// Validated forward-slash relative path.
    pub path: String,
    /// Observed metadata; absent if it could not be inspected safely.
    pub metadata: Option<EntryMetadata>,
    /// Explicit local problem, never a synthesized deletion.
    pub issue: Option<String>,
}

/// Complete metadata scan, or partial evidence with deletion disabled.
#[derive(Clone, Debug)]
pub struct ScanReport {
    /// Original root identity.
    pub root: FileIdentity,
    /// Aggregate enumeration health.
    pub health: ScanHealth,
    /// Entries and local errors; no content bytes are read.
    pub entries: Vec<ScanEntry>,
}

impl ScanReport {
    /// Only a full healthy enumeration can contribute missing-entry evidence.
    #[must_use]
    pub fn permits_missing_comparison(&self) -> bool {
        self.health == ScanHealth::Complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_incomplete_health_blocks_missing_comparison() {
        for health in [
            ScanHealth::PermissionDenied,
            ScanHealth::RootUnavailable,
            ScanHealth::Incomplete,
        ] {
            let report = ScanReport {
                root: FileIdentity {
                    volume: 1,
                    file: [0; 16],
                },
                health,
                entries: vec![],
            };
            assert!(!report.permits_missing_comparison());
        }
    }

    #[test]
    fn cloud_variants_are_recognized_but_mounts_and_symlinks_are_not() {
        let mut meta = EntryMetadata {
            identity: FileIdentity {
                volume: 1,
                file: [0; 16],
            },
            size: 0,
            allocated: 0,
            directory: false,
            attributes: 0x400,
            reparse_tag: 0,
            links: 1,
            created: 0,
            modified: 0,
            cloud: None,
        };
        for variant in 0..16 {
            meta.reparse_tag = 0x9000_001a | (variant << 12);
            assert!(meta.placeholder());
        }
        for tag in [0xa000_0003, 0xa000_000c, 0x8000_001b] {
            meta.reparse_tag = tag;
            assert!(!meta.supported());
        }
    }
}
