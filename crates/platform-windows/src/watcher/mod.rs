//! Notifications are hints only. Even a remove/rename notification needs a healthy scan.

#[cfg(windows)]
mod native;
#[cfg(windows)]
pub use native::DirectoryWatcher;

/// A notification path, with the original Win32 action preserved for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Validated relative path with forward slashes.
    pub path: String,
    /// FILE_ACTION_*; it never directly authorizes a Delete operation.
    pub action: u32,
}

/// A watcher requires full metadata reconciliation when hints are lost or ambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RescanReason {
    /// Native buffer returned zero bytes / ERROR_NOTIFY_ENUM_DIR.
    Overflow,
    /// A malformed, truncated or unsafe record cannot be trusted.
    InvalidNotification,
    /// Directory watch or root verification failed.
    WatchLost,
}

/// Result of a bounded native polling interval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchBatch {
    /// No completed request during the interval.
    Idle,
    /// Hints requiring metadata reconciliation.
    Changes(Vec<Change>),
    /// Lost hints requiring immediate full scan, preserving previous state until healthy.
    Rescan(RescanReason),
}

/// Decode FILE_NOTIFY_INFORMATION without assuming pointer alignment or trusting offsets.
#[must_use]
pub fn decode_notifications(bytes: &[u8]) -> WatchBatch {
    if bytes.is_empty() {
        return WatchBatch::Rescan(RescanReason::Overflow);
    }
    let mut changes = vec![];
    let mut offset = 0usize;
    loop {
        let Some(header) = bytes.get(offset..offset.saturating_add(12)) else {
            return invalid();
        };
        let next = u32::from_le_bytes(header[0..4].try_into().expect("four bytes")) as usize;
        let action = u32::from_le_bytes(header[4..8].try_into().expect("four bytes"));
        let length = u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) as usize;
        if !(1..=5).contains(&action) || length % 2 != 0 || length == 0 {
            return invalid();
        }
        let Some(name) = bytes.get(offset + 12..(offset + 12).saturating_add(length)) else {
            return invalid();
        };
        let wide: Vec<_> = name
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        let Ok(name) = String::from_utf16(&wide) else {
            return invalid();
        };
        let path = name.replace('\\', "/");
        if vaultsyncd_fs_common::path::RelativePath::parse(&path).is_err() {
            return invalid();
        }
        changes.push(Change { path, action });
        if next == 0 {
            return WatchBatch::Changes(changes);
        }
        if next % 4 != 0 || next < 12 + length || offset.checked_add(next).is_none() {
            return invalid();
        }
        offset += next;
    }
}

fn invalid() -> WatchBatch {
    WatchBatch::Rescan(RescanReason::InvalidNotification)
}

/// Periodic metadata scans cover dropped notifications and the watch rearming gap.
#[derive(Clone, Debug)]
pub struct ReconcileGate {
    next_scan_ms: u64,
    interval_ms: u64,
    required: bool,
}

impl ReconcileGate {
    /// Start with a scan required; zero intervals are clamped to one second.
    #[must_use]
    pub fn new(interval_ms: u64) -> Self {
        Self {
            next_scan_ms: 0,
            interval_ms: interval_ms.max(1000),
            required: true,
        }
    }

    /// All non-idle notifications, including remove hints, require a scan.
    pub fn observe(&mut self, batch: &WatchBatch) {
        if !matches!(batch, WatchBatch::Idle) {
            self.required = true;
        }
    }

    /// A failed scan remains due. Only healthy enumeration clears the gate.
    pub fn scanned(&mut self, report: &crate::files::ScanReport, now_ms: u64) {
        self.required = !report.permits_missing_comparison();
        self.next_scan_ms = now_ms.saturating_add(self.interval_ms);
    }

    /// The caller schedules this bounded metadata task; this gate never mutates files.
    #[must_use]
    pub fn scan_due(&self, now_ms: u64) -> bool {
        self.required || now_ms >= self.next_scan_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, action: u32) -> Vec<u8> {
        let name: Vec<_> = path.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut bytes = vec![0; 4];
        bytes.extend(action.to_le_bytes());
        bytes.extend((name.len() as u32).to_le_bytes());
        bytes.extend(name);
        bytes
    }

    #[test]
    fn removals_are_hints_and_overflow_and_bad_paths_force_scan() {
        assert_eq!(
            decode_notifications(&record("dir\\name.txt", 2)),
            WatchBatch::Changes(vec![Change {
                path: "dir/name.txt".into(),
                action: 2
            }])
        );
        assert_eq!(
            decode_notifications(&[]),
            WatchBatch::Rescan(RescanReason::Overflow)
        );
        for path in ["..\\outside", "C:\\outside", "bad/CON", "a\\..\\b"] {
            assert_eq!(decode_notifications(&record(path, 1)), invalid());
        }
        for size in 1..12 {
            assert_eq!(decode_notifications(&vec![0; size]), invalid());
        }
        let mut malformed = record("x", 1);
        malformed[..4].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(decode_notifications(&malformed), invalid());
    }

    #[test]
    fn only_healthy_scans_clear_gate_and_periodic_scan_covers_lost_hints() {
        use crate::files::{FileIdentity, ScanHealth, ScanReport};
        let mut gate = ReconcileGate::new(60000);
        let mut report = ScanReport {
            root: FileIdentity {
                volume: 1,
                file: [0; 16],
            },
            health: ScanHealth::PermissionDenied,
            entries: vec![],
        };
        gate.scanned(&report, 100);
        assert!(gate.scan_due(100));
        report.health = ScanHealth::Complete;
        gate.scanned(&report, 100);
        assert!(!gate.scan_due(101));
        assert!(gate.scan_due(60100));
        gate.observe(&WatchBatch::Rescan(RescanReason::WatchLost));
        assert!(gate.scan_due(101));
    }
}
