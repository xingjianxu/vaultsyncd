//! 受控应用的 SQLite 短事务适配；文件系统与网络不进入事务。
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, Transaction};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::{self, BaselineEntry, EntryKind, LocalEntry};
use vaultsyncd_client_store::intents::{self, LocalIntentKind, NewLocalIntent};
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId, VersionId};
use vaultsyncd_domain::state::{Dirty, Residency};
use vaultsyncd_domain::version::{Generation, Revision};
use vaultsyncd_transfer::ports::{LocalErrorKind, PortError, VersionPin};

use super::ApplySpec;
use crate::ports::local::{ApplyReceipt, ApplyRequest, ApplyStage};
use crate::recovery::{ContentEvidence, changed};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Base {
    entry: [u8; 16],
    parent: Option<[u8; 16]>,
    name: String,
    revision: u64,
    version: Option<[u8; 16]>,
}
impl Base {
    fn new(value: &BaselineEntry) -> Self {
        Self {
            entry: *value.entry.as_bytes(),
            parent: value.parent.map(|v| *v.as_bytes()),
            name: value.name.clone(),
            revision: value.revision.get(),
            version: value.content_version.map(|v| *v.as_bytes()),
        }
    }
    fn decode(&self) -> Result<BaselineEntry, PortError> {
        Ok(BaselineEntry {
            entry: EntryId::from_bytes(self.entry),
            parent: self.parent.map(EntryId::from_bytes),
            name: self.name.clone(),
            kind: EntryKind::File,
            revision: Revision::new(self.revision).map_err(db)?,
            content_version: self.version.map(VersionId::from_bytes),
            deleted: false,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LocalState {
    generation: u64,
    dirty: String,
    residency: String,
    fingerprint: Option<Vec<u8>>,
    identity: Option<Vec<u8>>,
}
impl LocalState {
    fn new(row: LocalEntry) -> Self {
        Self {
            generation: row.generation.get(),
            dirty: match row.dirty {
                Dirty::Clean => "clean",
                Dirty::Dirty => "dirty",
                Dirty::Snapshotting => "snapshotting",
                Dirty::AwaitingCommit => "awaiting_commit",
            }
            .to_owned(),
            residency: match row.residency {
                Residency::MetadataOnly => "metadata_only",
                Residency::Partial => "partial",
                Residency::Materialized => "materialized",
            }
            .to_owned(),
            fingerprint: row.fingerprint,
            identity: row.file_identity,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Journal {
    op: [u8; 16],
    binding: [u8; 16],
    target: String,
    before: Option<Base>,
    after: Base,
    expected: ContentEvidence,
    digest: [u8; 32],
    size: u64,
    staged: String,
    local_before: Option<LocalState>,
    intent: i64,
    pub completed: bool,
}
impl Journal {
    pub fn op(&self) -> OpId {
        OpId::from_bytes(self.op)
    }
    pub fn spec(&self) -> Result<ApplySpec, PortError> {
        let after = self.after.decode()?;
        let binding = BindingId::from_bytes(self.binding);
        let version = after
            .content_version
            .ok_or_else(|| invalid("缺少固定版本"))?;
        let spec = ApplySpec {
            op: self.op(),
            binding,
            target: vaultsyncd_fs_common::path::RelativePath::parse(&self.target).map_err(db)?,
            baseline_before: self.before.as_ref().map(Base::decode).transpose()?,
            baseline_after: after,
            pin: VersionPin::new(
                binding.to_string(),
                EntryId::from_bytes(self.after.entry).to_string(),
                version.to_string(),
                self.digest,
                self.size,
            )
            .map_err(db)?,
            expected: self.expected.clone(),
        };
        validate(&spec)?;
        Ok(spec)
    }
    pub fn request(&self) -> Result<ApplyRequest, PortError> {
        let spec = self.spec()?;
        Ok(ApplyRequest {
            op: spec.op,
            intent: self.intent,
            binding: spec.binding,
            entry: Some(spec.baseline_after.entry),
            target: spec.target,
            staged: Some(self.staged.clone()),
            version: spec.baseline_after.content_version,
            expected_digest: Some(self.digest),
            expected_generation: Generation::new(match &self.expected {
                ContentEvidence::Absent => 0,
                ContentEvidence::File { generation, .. } => *generation,
            })
            .map_err(db)?,
        })
    }
}

pub(super) struct ApplyStore {
    database: Arc<Database>,
}
impl ApplyStore {
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }
    pub async fn load(&self, op: OpId) -> Result<Option<Journal>, PortError> {
        let row: Option<(String, bool)> =
            sqlx::query_as("SELECT journal, completed FROM controlled_applies WHERE op_id = ?")
                .bind(op.as_bytes().as_slice())
                .fetch_optional(self.database.pool())
                .await
                .map_err(db)?;
        row.map(|(text, completed)| decode(text, completed))
            .transpose()
    }
    pub async fn pending(&self, binding: BindingId) -> Result<Vec<Journal>, PortError> {
        let unknown: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM local_intents i LEFT JOIN controlled_applies a ON a.intent_id = i.intent_id \
             WHERE i.binding_id = ? AND (a.op_id IS NULL OR a.completed <> 0)")
            .bind(binding.as_bytes().as_slice()).fetch_one(self.database.pool()).await.map_err(db)?;
        if unknown != 0 {
            return Err(invalid(
                "绑定仍有不能证明结果的旧意图或其它平台意图；禁止破坏性动作",
            ));
        }
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT journal, completed FROM controlled_applies WHERE binding_id = ? AND completed = 0 ORDER BY intent_id")
            .bind(binding.as_bytes().as_slice()).fetch_all(self.database.pool()).await.map_err(db)?;
        rows.into_iter()
            .map(|(text, completed)| decode(text, completed))
            .collect()
    }
    pub async fn prepare(&self, spec: &ApplySpec, staged: String) -> Result<Journal, PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let local = entries::local_entry(&mut tx, spec.binding, spec.baseline_after.entry)
            .await
            .map_err(db)?;
        if let Some(local) = &local {
            if local.dirty != Dirty::Clean
                || match &spec.expected {
                    ContentEvidence::Absent => true,
                    ContentEvidence::File {
                        generation,
                        identity,
                        ..
                    } => {
                        local.generation.get() != *generation
                            || local.file_identity.as_ref() != Some(identity)
                    }
                }
            {
                return Err(changed());
            }
        } else if !matches!(spec.expected, ContentEvidence::Absent) {
            return Err(changed());
        }
        let intent = intents::begin_intent(
            &mut tx,
            &NewLocalIntent {
                binding: spec.binding,
                kind: LocalIntentKind::Replace,
                entry: Some(spec.baseline_after.entry),
                target_path: spec.target.to_string(),
                source_path: Some(staged.clone()),
                content_hash: Some(spec.pin.digest.to_vec()),
                generation: match &spec.expected {
                    ContentEvidence::Absent => None,
                    ContentEvidence::File { generation, .. } => {
                        Some(Generation::new(*generation).map_err(db)?)
                    }
                },
            },
        )
        .await
        .map_err(db)?;
        let journal = Journal {
            op: *spec.op.as_bytes(),
            binding: *spec.binding.as_bytes(),
            target: spec.target.to_string(),
            before: spec.baseline_before.as_ref().map(Base::new),
            after: Base::new(&spec.baseline_after),
            expected: spec.expected.clone(),
            digest: spec.pin.digest,
            size: spec.pin.size,
            staged,
            local_before: local.map(LocalState::new),
            intent: intent.id,
            completed: false,
        };
        check_tx(&mut tx, &journal).await?;
        sqlx::query("INSERT INTO controlled_applies(op_id, binding_id, target_path, intent_id, journal) VALUES (?, ?, ?, ?, ?)")
            .bind(spec.op.as_bytes().as_slice()).bind(spec.binding.as_bytes().as_slice()).bind(&journal.target)
            .bind(intent.id).bind(serde_json::to_string(&journal).map_err(db)?)
            .execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(journal)
    }
    pub async fn check(&self, journal: &Journal) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        check_tx(&mut tx, journal).await?;
        tx.commit().await.map_err(db)
    }
    pub async fn finish(
        &self,
        journal: &Journal,
        actual: &ContentEvidence,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        check_tx(&mut tx, journal).await?;
        let spec = journal.spec()?;
        let receipt = receipt(actual)?;
        entries::put_baseline(&mut tx, spec.binding, &spec.baseline_after)
            .await
            .map_err(db)?;
        entries::put_local(
            &mut tx,
            spec.binding,
            &LocalEntry {
                entry: spec.baseline_after.entry,
                generation: receipt.generation,
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: Some(spec.pin.digest.to_vec()),
                file_identity: receipt.file_identity,
            },
        )
        .await
        .map_err(db)?;
        let updated = sqlx::query("UPDATE controlled_applies SET completed = 1, intent_id = NULL WHERE op_id = ? AND completed = 0 AND intent_id = ?")
            .bind(&journal.op[..]).bind(journal.intent).execute(&mut *tx).await.map_err(db)?;
        if updated.rows_affected() != 1
            || !intents::complete_intent(&mut tx, journal.intent)
                .await
                .map_err(db)?
        {
            return Err(invalid("应用意图缺失或已被其它执行器完成"));
        }
        tx.commit().await.map_err(db)
    }
}

async fn check_tx(tx: &mut Transaction<'_, Sqlite>, journal: &Journal) -> Result<(), PortError> {
    let spec = journal.spec()?;
    let base = entries::baseline_entry(tx, spec.binding, spec.baseline_after.entry)
        .await
        .map_err(db)?;
    let local = entries::local_entry(tx, spec.binding, spec.baseline_after.entry)
        .await
        .map_err(db)?;
    if journal.completed {
        if base != Some(spec.baseline_after)
            || local.as_ref().is_none_or(|row| {
                row.dirty != Dirty::Clean
                    || row.fingerprint.as_deref() != Some(journal.digest.as_slice())
            })
        {
            return Err(changed());
        }
    } else {
        if base != spec.baseline_before || local.map(LocalState::new) != journal.local_before {
            return Err(changed());
        }
        let intent = intents::intent(tx, journal.intent)
            .await
            .map_err(db)?
            .ok_or_else(|| invalid("待恢复意图丢失"))?;
        if intent.state != intents::IntentState::Pending
            || intent.binding != spec.binding
            || intent.target_path != journal.target
            || intent.kind != LocalIntentKind::Replace
            || intent.content_hash.as_deref() != Some(journal.digest.as_slice())
            || intent.source_path.as_deref() != Some(journal.staged.as_str())
            || intent.entry != Some(spec.baseline_after.entry)
            || intent.generation.map(|g| g.get())
                != match &journal.expected {
                    ContentEvidence::Absent => None,
                    ContentEvidence::File { generation, .. } => Some(*generation),
                }
        {
            return Err(invalid("应用意图不匹配或已停止"));
        }
    }
    Ok(())
}

pub(super) fn receipt(actual: &ContentEvidence) -> Result<ApplyReceipt, PortError> {
    match actual {
        ContentEvidence::File {
            generation,
            identity,
            ..
        } => Ok(ApplyReceipt {
            stage: ApplyStage::Unchanged,
            generation: Generation::new(*generation).map_err(db)?,
            file_identity: Some(identity.clone()),
        }),
        ContentEvidence::Absent => Err(changed()),
    }
}
fn decode(text: String, completed: bool) -> Result<Journal, PortError> {
    let mut journal: Journal = serde_json::from_str(&text).map_err(db)?;
    journal.completed = completed;
    journal.spec()?;
    Ok(journal)
}
pub(super) fn validate(spec: &ApplySpec) -> Result<(), PortError> {
    let after = &spec.baseline_after;
    if after.kind != EntryKind::File
        || after.deleted
        || after.parent.is_none()
        || after.content_version.is_none()
        || spec.pin.binding != spec.binding.to_string()
        || spec.pin.entry != after.entry.to_string()
        || Some(spec.pin.version.clone()) != after.content_version.map(|v| v.to_string())
        || spec.target.to_string().rsplit('/').next() != Some(after.name.as_str())
        || spec.baseline_before.as_ref().is_some_and(|before| {
            before.entry != after.entry
                || before.kind != EntryKind::File
                || before.deleted
                || before.parent.is_none()
                || before.revision >= after.revision
        })
    {
        return Err(invalid(
            "受控应用必须固定已提交文件版本、合法位置和前后基线",
        ));
    }
    Ok(())
}
fn invalid(detail: &str) -> PortError {
    PortError::rejected("NEEDS_RECOVERY", detail)
}
fn db(error: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Read, error.to_string())
}
