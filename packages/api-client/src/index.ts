// packages/api-client 的稳定导出入口（docs/design.md §5.6）。
//
// src/generated/ 由 `cargo xtask generate` 从 api/openapi.yaml 生成，禁止手改；本文件是手写的
// 稳定边界：Web 应用只从这里取类型与调用函数，生成代码的文件名/拆分方式变化不会波及调用方。
export { ApiError, ProtocolError, getCapabilities, getHealth, listEntryHistory, listTrash } from "./generated/client";
export { browserLogin, browserRefresh, getAccount, changePassword, revokeAllSessions, logout } from "./generated/client";
export type { AccountInfo, AccountChanged, BrowserSession } from "./generated/types";
export type { ClientOptions, FetchLike } from "./generated/client";
export {
  decodeRecoveryItem,
  decodeRecoveryPage,
  decodeCapabilities,
  decodeErrorCode,
  decodeErrorEnvelope,
  decodeHealthResponse,
  decodeHealthStatus,
} from "./generated/types";
export type {
  RecoveryItem,
  RecoveryPage,
  Capabilities,
  CapabilityLimits,
  ErrorBody,
  ErrorCode,
  ErrorEnvelope,
  HealthResponse,
  HealthStatus,
  ProtocolVersion,
} from "./generated/types";

export { listManagedRoots, registerRoot } from "./generated/client";
export type { ManagedRootList, ManagedRoot, RootCollection, RootCollectionState, RootRegistered, RootRegisterRequest } from "./generated/types";

export { listRecoveryEntries, listConflictCopies, listRoots, createSnapshot, submitOperation, getOperation } from "./generated/client";
export type { OperationRequest, OperationResult, RootSummary } from "./generated/types";
export { listSnapshotEntries } from "./generated/client";
export type { Snapshot, SnapshotEntry, SnapshotPage } from "./generated/types";

export { beginUpload, getUpload, putUploadPart, finalizeUpload, getSubtreeToken, downloadVersionContent, acquireVersionLease, renewVersionLease, releaseVersionLease } from "./generated/client";
export type { UploadRequest, UploadSession, UploadStatus, UploadPartAck, FinalizedContent, SubtreeToken, VersionLease, VersionLeaseRequest } from "./generated/types";

export { listBackupSnapshots, prepareBackupSnapshot, backupSnapshotStatus, verifyBackupSnapshot, releaseBackupSnapshot, backupSnapshotManifest, exportBackupSnapshot } from "./generated/client";
export type { BackupSnapshotStatus, BackupSnapshotList, BackupIntegrity, BackupSnapshotPhase } from "./generated/types";
