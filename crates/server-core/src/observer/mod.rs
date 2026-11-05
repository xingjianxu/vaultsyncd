//! 服务端外部目录观察（design §12.1—12.2）：有界扫描、稳定窗口、私有候选采集与采纳。
//!
//! 本模块只放规则与顺序，不做平台 I/O：枚举与指纹走 [`crate::ports::work_tree::WorkTreePort`]，
//! 候选与对象走 [`crate::ports::object_store::ObjectBlobPort`]，采纳事务走
//! [`crate::ports::external_store::ExternalAdoptStore`]。

pub mod capture;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod import;
pub mod scan;

pub use capture::{
    AdoptFailure, AdoptOutcome, BlockReason, CaptureOutcome, ExternalCaptureError,
    ExternalProposal, RequeueReason, adopt_external, capture_external,
};
pub use import::{
    DeferReason, ExternalObserver, FileOutcome, FileState, ObserverConfigError, ObserverPorts,
    PassClock, PassLimits, PassReport, PendingReason,
};
pub use scan::{
    InconclusiveReason, ScanCompletion, ScanCursor, ScanLimits, ScanReport, ScannedFile,
    SkipReason, SkippedEntry, Stability, StabilityTracker, StabilityWindow, scan_tree,
};
