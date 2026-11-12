//! Read-only file projection from actual safe scan and durable facts; never authorizes release.
use crate::{
    local::error,
    management::Running,
    work::{metadata, paths},
};
use std::{collections::BTreeMap, sync::Arc};
use vaultsyncd_agent_core::bindings::control::{BindingControls, ExecutionGate};
use vaultsyncd_client_store::{db::Database, entries};
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{BindingId, EntryId},
    state::{Dirty, EntryState, EntryStatus, RemoteApply, Residency, Transfer, Usage},
};
use vaultsyncd_local_ipc::generated::{FileStatus, ManagementResult};
use vaultsyncd_platform_windows::files::controlled::ProtectedFile;
use vaultsyncd_transfer::{
    hash::{digest_of, to_hex},
    ports::PortError,
};

pub(crate) async fn list(
    db: &Arc<Database>,
    running: &Running,
    binding: BindingId,
    cursor: Option<&str>,
) -> Result<ManagementResult, PortError> {
    let (worker, _) = running
        .get(&binding)
        .ok_or_else(|| PortError::rejected("BINDING_BLOCKED", "binding not active"))?;
    let local = &worker.importer.local;
    let scan = local.vault.scan();
    let complete = scan.permits_missing_comparison();
    if scan.health == vaultsyncd_platform_windows::files::ScanHealth::VaultUnavailable {
        return Err(PortError::rejected(
            "VAULT_UNAVAILABLE",
            "vault unavailable",
        ));
    }
    let issues: Vec<String> = scan
        .entries
        .iter()
        .filter(|e| e.issue.is_some())
        .map(|e| e.path.clone())
        .collect();
    let mut physical: BTreeMap<_, _> = scan
        .entries
        .into_iter()
        .filter_map(|e| e.metadata.filter(|m| !m.directory).map(|m| (e.path, m)))
        .collect();
    let rows = metadata(&worker.importer).await?;
    let names = paths(&rows)?;
    let controls = BindingControls::new(db.clone());
    let mut conn = db.pool().acquire().await.map_err(error)?;
    let info = vaultsyncd_client_store::bindings::binding(&mut conn, binding)
        .await
        .map_err(error)?
        .ok_or_else(|| error("missing binding"))?;
    drop(conn);
    let mut result = Vec::new();
    for (id, row) in &rows {
        if row.kind != vaultsyncd_protocol::generated::EntryKind::File {
            continue;
        }
        let path = names.get(id).ok_or_else(|| error("missing safe path"))?;
        let entry = EntryId::from_hex(id).map_err(error)?;
        let actual = physical.remove(path);
        let mut conn = db.pool().acquire().await.map_err(error)?;
        let persisted = entries::local_entry(&mut conn, binding, entry)
            .await
            .map_err(error)?;
        let base = entries::baseline_entry(&mut conn, binding, entry)
            .await
            .map_err(error)?
            .ok_or_else(|| error("missing baseline"))?;
        let remote = entries::remote_entry(&mut conn, binding, entry)
            .await
            .map_err(error)?;
        drop(conn);
        let mut tx = db.pool().begin().await.map_err(error)?;
        let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_ops WHERE binding_id=? AND (entry_id=? OR entry_id IS NULL)) OR EXISTS(SELECT 1 FROM local_intents WHERE binding_id=? AND (entry_id=? OR entry_id IS NULL))")
            .bind(binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).fetch_one(&mut *tx).await.map_err(error)?;
        let failed: Option<String> = sqlx::query_scalar("SELECT last_error_code FROM pending_ops WHERE binding_id=? AND (entry_id=? OR entry_id IS NULL) AND last_error_code IS NOT NULL LIMIT 1")
            .bind(binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).fetch_optional(&mut *tx).await.map_err(error)?;
        let offline: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operation_schedule s JOIN pending_ops p USING(op_id) WHERE p.binding_id=? AND (p.entry_id=? OR p.entry_id IS NULL) AND s.offline=1)")
            .bind(binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).fetch_one(&mut *tx).await.map_err(error)?;
        let transferring: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfers t JOIN baseline_entries b ON b.binding_id=t.binding_id AND b.content_version_id=t.version_id WHERE b.binding_id=? AND b.entry_id=? AND t.state='running')")
            .bind(binding.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).fetch_one(&mut *tx).await.map_err(error)?;
        tx.commit().await.map_err(error)?;
        let mut state = EntryState {
            dirty: persisted.as_ref().map(|e| e.dirty).unwrap_or(Dirty::Dirty),
            residency: persisted
                .as_ref()
                .map(|e| e.residency)
                .unwrap_or(Residency::Partial),
            ..Default::default()
        };
        let content_evidence = persisted.as_ref().is_some_and(|p| {
            p.fingerprint.as_ref().is_some_and(|v| !v.is_empty())
                && p.file_identity.as_ref().is_some_and(|v| !v.is_empty())
        });
        let (residency, busy) = if let Some(actual) = &actual {
            if row
                .size
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|s| s != actual.size)
            {
                state.dirty = Dirty::Dirty;
            }
            if let Some(cloud) = &actual.cloud {
                if cloud.modified > 0 {
                    state.dirty = Dirty::Dirty;
                }
                state.residency = if cloud.on_disk == 0 && actual.size > 0 {
                    Residency::MetadataOnly
                } else if cloud.on_disk < actual.size {
                    Residency::Partial
                } else {
                    Residency::Materialized
                };
            } else if state.residency != Residency::Materialized {
                state.dirty = Dirty::Dirty;
            }
            let busy = state.residency == Residency::Materialized
                && ProtectedFile::open_under(&local.vault, path, true).is_err();
            (
                match state.residency {
                    Residency::MetadataOnly => "metadata_only",
                    Residency::Partial => "partial",
                    Residency::Materialized => "materialized",
                },
                busy,
            )
        } else {
            state.availability = vaultsyncd_domain::state::Availability::VaultUnavailable;
            ("unknown", true)
        };
        if remote.is_some_and(|r| {
            r.revision != base.revision
                || r.content_version != base.content_version
                || r.deleted != base.deleted
        }) {
            state.remote_apply = RemoteApply::Pending;
        }
        if state.residency == Residency::Materialized && !content_evidence {
            state.remote_apply = RemoteApply::Pending;
        }
        if pending {
            state.transfer = Transfer::Queued;
        }
        if transferring {
            state.transfer = Transfer::Running;
        }
        if offline {
            state.availability = vaultsyncd_domain::state::Availability::Offline;
        }
        if let Some(code) = failed {
            if code.contains("CONFLICT") {
                state.remote_apply = RemoteApply::Conflicted;
            } else {
                let code = ErrorCode::from_code(&code).unwrap_or(ErrorCode::NeedsRecovery);
                state.transfer = if code.class().is_severe() {
                    Transfer::Failed(code)
                } else {
                    Transfer::RetryWait(code)
                };
            }
        }
        if !complete {
            state.availability = vaultsyncd_domain::state::Availability::VaultUnavailable;
        }
        state.usage = if busy { Usage::Unknown } else { Usage::Idle };
        let gate = controls.execution_gate(binding, Some(entry)).await?;
        let reason = if !complete {
            "扫描未完成，文件可能正在使用或不可访问，不能释放"
        } else if actual.is_none()
            && (gate == ExecutionGate::Excluded || gate == ExecutionGate::ReconcileRequired)
        {
            "已跟踪文件在排除期间被本地删除；不会传播删除。取消排除后点击下载显式恢复"
        } else if gate == ExecutionGate::ReconcileRequired && state.residency == Residency::Partial
        {
            "部分占位仍需按原固定版本核对；点击下载恢复，已修改内容不会被覆盖"
        } else if actual.is_none() {
            "实际文件可能正在使用、不可访问或尚未核对"
        } else if state.dirty != Dirty::Clean {
            "存在未确认本地修改，不能释放"
        } else if state.residency == Residency::Materialized && !content_evidence {
            "尚未取得当前完整内容的确认凭据，不能释放"
        } else if busy {
            "文件正在使用或无法取得保护句柄，不能释放"
        } else if pending
            || state.remote_apply != RemoteApply::Current
            || state.transfer != Transfer::Idle
        {
            "存在未完成动作或远端变化，不能释放"
        } else if state.residency != Residency::Materialized {
            "本地不是完整已下载内容"
        } else if info.mode == vaultsyncd_client_store::bindings::SyncMode::Full {
            "完整同步模式要求保留离线内容"
        } else if gate == ExecutionGate::Excluded {
            "路径被绑定排除规则排除；本地与服务器内容保留"
        } else if gate == ExecutionGate::ReconcileRequired {
            "取消排除后需要核对最新远端与本地，尚未完成重新协调"
        } else if gate != ExecutionGate::Ready {
            "绑定已暂停、解除或需要核对"
        } else {
            ""
        };
        let status = match state.project() {
            EntryStatus::Unavailable => "unavailable",
            EntryStatus::Failed => "failed",
            EntryStatus::Conflict => "conflict",
            EntryStatus::LocalChanges => "local_changes",
            EntryStatus::Syncing => "syncing",
            EntryStatus::Downloaded => "downloaded",
            EntryStatus::CloudOnly => "cloud_only",
        };
        result.push(FileStatus {
            entry_id: Some(id.clone()),
            path: path.clone(),
            status: status.into(),
            availability: match state.availability {
                vaultsyncd_domain::state::Availability::Online => "online",
                vaultsyncd_domain::state::Availability::Offline => "offline",
                _ => "vault_unavailable",
            }
            .into(),
            remote_apply: match state.remote_apply {
                RemoteApply::Current => "current",
                RemoteApply::Pending => "pending",
                RemoteApply::BlockedByLocal => "blocked_by_local",
                RemoteApply::Conflicted => "conflicted",
            }
            .into(),
            transfer: match state.transfer {
                Transfer::Idle => "idle",
                Transfer::Queued => "queued",
                Transfer::Running => "running",
                Transfer::RetryWait(_) => "retry_wait",
                Transfer::Failed(_) => "failed",
            }
            .into(),
            residency: residency.into(),
            dirty: state.dirty != Dirty::Clean,
            busy,
            pending: pending || transferring || state.remote_apply != RemoteApply::Current,
            synchronized: state.is_synced()
                && !pending
                && !transferring
                && actual.is_some()
                && gate == ExecutionGate::Ready
                && complete,
            release_allowed: reason.is_empty(),
            release_reason: if reason.is_empty() {
                "释放本地内容后远端保留，离线将不可读".into()
            } else {
                reason.into()
            },
        });
    }
    for (path, _) in physical {
        result.push(FileStatus {
            entry_id: None,
            path,
            status: "local_changes".into(),
            availability: if complete {
                "online"
            } else {
                "vault_unavailable"
            }
            .into(),
            remote_apply: "pending".into(),
            transfer: "queued".into(),
            residency: "unknown".into(),
            dirty: true,
            busy: false,
            pending: true,
            synchronized: false,
            release_allowed: false,
            release_reason: "本地内容尚未确认身份或版本，不能释放".into(),
        });
    }
    for path in issues {
        if !result.iter().any(|r| r.path == path) {
            result.push(FileStatus {
                entry_id: None,
                path,
                status: "unavailable".into(),
                availability: "vault_unavailable".into(),
                remote_apply: "pending".into(),
                transfer: "queued".into(),
                residency: "unknown".into(),
                dirty: true,
                busy: true,
                pending: true,
                synchronized: false,
                release_allowed: false,
                release_reason: "文件不可访问或扫描未完成，内容未被当作删除".into(),
            });
        }
    }
    let mut keyed: Vec<_> = result
        .into_iter()
        .map(|row| {
            (
                to_hex(&digest_of(row.path.as_bytes()))[..32].to_owned(),
                row,
            )
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    if keyed.windows(2).any(|p| p[0].0 == p[1].0) {
        return Err(error("file listing identity collision"));
    }
    keyed.retain(|(key, _)| cursor.is_none_or(|c| key.as_str() > c));
    let next_cursor = (keyed.len() > 100).then(|| keyed[99].0.clone());
    Ok(ManagementResult {
        files: Some(keyed.into_iter().take(100).map(|(_, row)| row).collect()),
        next_cursor,
        ..Default::default()
    })
}
