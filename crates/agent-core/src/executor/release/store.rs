//! 释放持久化适配器；只使用SQLite短事务，不在事务内哈希、访问平台或服务器。
use super::{ReleaseSpec, ReleasedFile, reject};
use crate::recovery::ContentEvidence;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};
use std::collections::HashSet;
use std::sync::Arc;
use vaultsyncd_client_store::{
    db::Database,
    entries::{self, LocalEntry},
    intents::{self, LocalIntentKind, NewLocalIntent},
};
use vaultsyncd_domain::{
    ids::{BindingId, EntryId, OpId, VersionId},
    state::{Dirty, Residency},
    version::Generation,
};
use vaultsyncd_transfer::ports::{LocalErrorKind, PortError, VersionPin};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    generation: u64,
    dirty: String,
    residency: String,
    fingerprint: Option<Vec<u8>>,
    identity: Option<Vec<u8>>,
    base_parent: Option<Vec<u8>>,
    base_name: String,
    base_revision: i64,
    base_version: Vec<u8>,
    mode: String,
    epoch: i64,
    control_revision: i64,
}
impl Snapshot {
    pub fn matches_content(
        &self,
        content: &ContentEvidence,
        pin: &VersionPin,
    ) -> Result<(), PortError> {
        match content {
            ContentEvidence::File {
                generation,
                identity,
                digest,
                size,
            } if *generation == self.generation
                && self.identity.as_ref() == Some(identity)
                && *digest == pin.digest
                && *size == pin.size =>
            {
                Ok(())
            }
            _ => Err(crate::recovery::changed()),
        }
    }
    fn local(&self, entry: EntryId, residency: Residency) -> Result<LocalEntry, PortError> {
        Ok(LocalEntry {
            entry,
            generation: Generation::new(self.generation).map_err(error)?,
            dirty: Dirty::Clean,
            residency,
            fingerprint: self.fingerprint.clone(),
            file_identity: self.identity.clone(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Journal {
    op: [u8; 16],
    binding: [u8; 16],
    entry: [u8; 16],
    target: String,
    version: [u8; 16],
    digest: [u8; 32],
    size: u64,
    before: Snapshot,
    pub evidence: ContentEvidence,
    intent: i64,
    #[serde(skip)]
    pub completed: bool,
}
impl Journal {
    pub fn spec(&self) -> Result<ReleaseSpec, PortError> {
        let binding = BindingId::from_bytes(self.binding);
        let entry = EntryId::from_bytes(self.entry);
        Ok(ReleaseSpec {
            op: OpId::from_bytes(self.op),
            binding,
            entry,
            path: vaultsyncd_fs_common::path::RelativePath::parse(&self.target).map_err(error)?,
            pin: VersionPin::new(
                binding.to_string(),
                entry.to_string(),
                VersionId::from_bytes(self.version).to_string(),
                self.digest,
                self.size,
            )
            .map_err(error)?,
        })
    }
    pub fn matches_released(&self, receipt: &ReleasedFile) -> Result<(), PortError> {
        if receipt.generation != self.before.generation
            || self.before.identity.as_ref() != Some(&receipt.identity)
            || receipt.pin != self.spec()?.pin
        {
            return Err(crate::recovery::changed());
        }
        Ok(())
    }
}

pub(super) struct Store {
    pub database: Arc<Database>,
}
impl Store {
    pub async fn require_on_demand(&self, binding: BindingId) -> Result<(), PortError> {
        let mode: Option<String> = sqlx::query_scalar("SELECT mode FROM bindings WHERE id=?")
            .bind(binding.as_bytes().as_slice())
            .fetch_optional(self.database.pool())
            .await
            .map_err(error)?;
        match mode.as_deref() {
            Some("on_demand") => Ok(()),
            Some("full") => Err(reject(
                "MODE_REQUIRES_LOCAL_COPY",
                "请先切换为按需模式；full要求保留完整内容",
            )),
            _ => Err(reject("BINDING_MISSING", "绑定不存在或模式无效")),
        }
    }
    pub async fn load(&self, op: OpId) -> Result<Option<Journal>, PortError> {
        let row=sqlx::query("SELECT op_id,binding_id,target_path,intent_id,journal,completed FROM controlled_releases WHERE op_id=?")
            .bind(op.as_bytes().as_slice()).fetch_optional(self.database.pool()).await.map_err(error)?;
        row.map(decode_row).transpose()
    }
    pub async fn pending(&self, binding: BindingId) -> Result<Vec<Journal>, PortError> {
        let unknown:i64=sqlx::query_scalar("SELECT count(*) FROM local_intents i LEFT JOIN controlled_releases r ON r.intent_id=i.intent_id WHERE i.binding_id=? AND (r.op_id IS NULL OR r.completed<>0)")
            .bind(binding.as_bytes().as_slice()).fetch_one(self.database.pool()).await.map_err(error)?;
        if unknown > 0 {
            return Err(reject("UNKNOWN_LOCAL_INTENT", "需先恢复其它本地意图"));
        }
        let rows=sqlx::query("SELECT op_id,binding_id,target_path,intent_id,journal,completed FROM controlled_releases WHERE binding_id=? AND completed=0 ORDER BY intent_id")
            .bind(binding.as_bytes().as_slice()).fetch_all(self.database.pool()).await.map_err(error)?;
        rows.into_iter().map(decode_row).collect()
    }
    pub async fn snapshot(&self, spec: &ReleaseSpec) -> Result<Snapshot, PortError> {
        let mut tx = self.database.pool().begin().await.map_err(error)?;
        let snapshot = snapshot(&mut tx, spec, None, false).await?;
        tx.commit().await.map_err(error)?;
        Ok(snapshot)
    }
    pub async fn prepare(
        &self,
        spec: &ReleaseSpec,
        before: &Snapshot,
        evidence: ContentEvidence,
    ) -> Result<Journal, PortError> {
        let mut tx = self.database.begin_write().await.map_err(error)?;
        if &snapshot(&mut tx, spec, None, false).await? != before {
            return Err(crate::recovery::changed());
        }
        before.matches_content(&evidence, &spec.pin)?;
        let intent = intents::begin_intent(
            &mut tx,
            &NewLocalIntent {
                binding: spec.binding,
                kind: LocalIntentKind::Release,
                entry: Some(spec.entry),
                target_path: spec.path.to_string(),
                source_path: None,
                content_hash: Some(spec.pin.digest.to_vec()),
                generation: Some(Generation::new(before.generation).map_err(error)?),
            },
        )
        .await
        .map_err(error)?
        .id;
        let journal = Journal {
            op: *spec.op.as_bytes(),
            binding: *spec.binding.as_bytes(),
            entry: *spec.entry.as_bytes(),
            target: spec.path.to_string(),
            version: *VersionId::from_hex(&spec.pin.version)
                .map_err(error)?
                .as_bytes(),
            digest: spec.pin.digest,
            size: spec.pin.size,
            before: before.clone(),
            evidence,
            intent,
            completed: false,
        };
        sqlx::query("INSERT INTO controlled_releases(op_id,binding_id,target_path,intent_id,journal) VALUES (?,?,?,?,?)")
            .bind(spec.op.as_bytes().as_slice()).bind(spec.binding.as_bytes().as_slice()).bind(spec.path.to_string()).bind(intent)
            .bind(serde_json::to_string(&journal).map_err(error)?).execute(&mut *tx).await.map_err(error)?;
        tx.commit().await.map_err(error)?;
        Ok(journal)
    }
    pub async fn check(&self, journal: &Journal) -> Result<(), PortError> {
        let mut tx = self.database.pool().begin().await.map_err(error)?;
        check(&mut tx, journal).await?;
        tx.commit().await.map_err(error)
    }
    pub async fn finish(&self, journal: &Journal, receipt: &ReleasedFile) -> Result<(), PortError> {
        journal.matches_released(receipt)?;
        let spec = journal.spec()?;
        let mut tx = self.database.begin_write().await.map_err(error)?;
        check(&mut tx, journal).await?;
        entries::put_local(
            &mut tx,
            spec.binding,
            &journal.before.local(spec.entry, Residency::MetadataOnly)?,
        )
        .await
        .map_err(error)?;
        let result=sqlx::query("UPDATE controlled_releases SET completed=1,intent_id=NULL WHERE op_id=? AND completed=0 AND intent_id=?")
            .bind(spec.op.as_bytes().as_slice()).bind(journal.intent).execute(&mut *tx).await.map_err(error)?;
        if result.rows_affected() != 1 {
            return Err(reject("RELEASE_CHANGED", "释放日志已变化"));
        }
        if !intents::complete_intent(&mut tx, journal.intent)
            .await
            .map_err(error)?
        {
            return Err(reject("UNKNOWN_LOCAL_INTENT", "释放意图丢失"));
        }
        tx.commit().await.map_err(error)
    }
}
async fn check(conn: &mut SqliteConnection, journal: &Journal) -> Result<(), PortError> {
    let spec = journal.spec()?;
    let current = snapshot(
        conn,
        &spec,
        if journal.completed {
            None
        } else {
            Some(journal.intent)
        },
        journal.completed,
    )
    .await?;
    let mut expected = journal.before.clone();
    if journal.completed {
        expected.residency = "metadata_only".into();
    }
    if current != expected {
        return Err(crate::recovery::changed());
    }
    Ok(())
}
async fn snapshot(
    conn: &mut SqliteConnection,
    spec: &ReleaseSpec,
    own_intent: Option<i64>,
    completed: bool,
) -> Result<Snapshot, PortError> {
    use sqlx::Row;
    let binding = spec.binding.as_bytes().as_slice();
    let entry = spec.entry.as_bytes().as_slice();
    let row=sqlx::query("SELECT b.mode,b.server_epoch,COALESCE(c.revision,0) AS revision,COALESCE(c.paused,0) AS paused,COALESCE(c.detached,0) AS detached FROM bindings b LEFT JOIN binding_controls c ON c.binding_id=b.id WHERE b.id=?")
        .bind(binding).fetch_optional(&mut *conn).await.map_err(error)?.ok_or_else(||reject("BINDING_MISSING","绑定不存在"))?;
    let mode: String = row.try_get("mode").map_err(error)?;
    if mode == "full" {
        return Err(reject(
            "MODE_REQUIRES_LOCAL_COPY",
            "full要求保留本地完整副本",
        ));
    }
    if mode != "on_demand"
        || row.try_get::<bool, _>("paused").map_err(error)?
        || row.try_get::<bool, _>("detached").map_err(error)?
    {
        return Err(reject("BINDING_BLOCKED", "绑定暂停或已停止管理"));
    }
    let local = entries::local_entry(conn, spec.binding, spec.entry)
        .await
        .map_err(error)?
        .ok_or_else(crate::recovery::changed)?;
    if local.dirty != Dirty::Clean
        || local.residency
            != if completed {
                Residency::MetadataOnly
            } else {
                Residency::Materialized
            }
        || local.fingerprint.as_ref().is_none_or(Vec::is_empty)
        || local.file_identity.as_ref().is_none_or(Vec::is_empty)
    {
        return Err(reject(
            "LOCAL_NOT_CONFIRMED",
            "未确认、部分驻留或未知内容不能释放",
        ));
    }
    let base = entries::baseline_entry(conn, spec.binding, spec.entry)
        .await
        .map_err(error)?
        .ok_or_else(crate::recovery::changed)?;
    if base.deleted
        || base.kind != entries::EntryKind::File
        || base.content_version.map(|v| v.to_string()).as_deref() != Some(spec.pin.version.as_str())
    {
        return Err(reject("VERSION_MISMATCH", "本地基线没有确认该固定文件版本"));
    }
    if path(conn, spec.binding, spec.entry).await? != spec.path.to_string() {
        return Err(crate::recovery::changed());
    }
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM local_intents WHERE binding_id=? AND (? IS NULL OR intent_id<>?)",
    )
    .bind(binding)
    .bind(own_intent)
    .bind(own_intent)
    .fetch_one(&mut *conn)
    .await
    .map_err(error)?;
    if others > 0 {
        return Err(reject("UNKNOWN_LOCAL_INTENT", "仍有未恢复本地意图"));
    }
    if let Some(intent) = own_intent {
        let owned:i64=sqlx::query_scalar("SELECT count(*) FROM local_intents WHERE intent_id=? AND binding_id=? AND entry_id=? AND kind='release' AND target_path=? AND generation=? AND content_hash=? AND state='pending'")
            .bind(intent).bind(binding).bind(entry).bind(spec.path.to_string()).bind(local.generation.to_db_integer()).bind(spec.pin.digest.as_slice())
            .fetch_one(&mut *conn).await.map_err(error)?;
        if owned != 1 {
            return Err(reject("UNKNOWN_LOCAL_INTENT", "释放意图的身份或状态不符"));
        }
    }
    // This includes dirty/conflict candidates awaiting planning/commit, and unknown new-file operations.
    let own: Option<(Vec<u8>, Option<Vec<u8>>, String)> =
        sqlx::query_as("SELECT binding_id,entry_id,kind FROM pending_ops WHERE op_id=?")
            .bind(spec.op.as_bytes().as_slice())
            .fetch_optional(&mut *conn)
            .await
            .map_err(error)?;
    if own.is_some_and(|(b, e, kind)| {
        b != binding || e.as_deref() != Some(entry) || kind != "release_local"
    }) {
        return Err(reject("OP_ID_REUSED", "该OpId已用于其他动作/目标"));
    }
    let pending:i64=sqlx::query_scalar("SELECT count(*) FROM pending_ops WHERE binding_id=? AND op_id<>? AND (entry_id=? OR entry_id IS NULL)")
        .bind(binding).bind(spec.op.as_bytes().as_slice()).bind(entry).fetch_one(&mut *conn).await.map_err(error)?;
    if pending > 0 {
        return Err(reject(
            "PENDING_LOCAL_ACTION",
            "目标仍有未确认动作或冲突候选",
        ));
    }
    Ok(Snapshot {
        generation: local.generation.get(),
        dirty: "clean".into(),
        residency: if completed {
            "metadata_only"
        } else {
            "materialized"
        }
        .into(),
        fingerprint: local.fingerprint,
        identity: local.file_identity,
        base_parent: base.parent.map(|v| v.as_bytes().to_vec()),
        base_name: base.name,
        base_revision: base.revision.to_db_integer(),
        base_version: base
            .content_version
            .expect("已核对版本")
            .as_bytes()
            .to_vec(),
        mode,
        epoch: row.try_get("server_epoch").map_err(error)?,
        control_revision: row.try_get("revision").map_err(error)?,
    })
}
async fn path(
    conn: &mut SqliteConnection,
    binding: BindingId,
    entry: EntryId,
) -> Result<String, PortError> {
    let mut current = entry;
    let mut names = vec![];
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return Err(reject("INVALID_NAMESPACE", "父身份成环"));
        }
        let row = entries::baseline_entry(conn, binding, current)
            .await
            .map_err(error)?
            .ok_or_else(|| reject("INVALID_NAMESPACE", "父身份未知"))?;
        if row.deleted {
            return Err(crate::recovery::changed());
        }
        match row.parent {
            Some(parent) => {
                names.push(row.name);
                current = parent;
            }
            None => break,
        }
    }
    names.reverse();
    Ok(names.join("/"))
}
fn decode_row(row: SqliteRow) -> Result<Journal, PortError> {
    let json: String = row.try_get("journal").map_err(error)?;
    let completed: bool = row.try_get("completed").map_err(error)?;
    let journal = decode(&json, completed)?;
    if row.try_get::<Vec<u8>, _>("op_id").map_err(error)? != journal.op
        || row.try_get::<Vec<u8>, _>("binding_id").map_err(error)? != journal.binding
        || row.try_get::<String, _>("target_path").map_err(error)? != journal.target
        || row.try_get::<Option<i64>, _>("intent_id").map_err(error)?
            != if completed {
                None
            } else {
                Some(journal.intent)
            }
    {
        return Err(reject("INVALID_RELEASE_JOURNAL", "日志行与内容身份不符"));
    }
    Ok(journal)
}
fn decode(json: &str, completed: bool) -> Result<Journal, PortError> {
    let mut journal: Journal = serde_json::from_str(json).map_err(error)?;
    journal.completed = completed;
    super::validate(&journal.spec()?)?;
    journal
        .before
        .matches_content(&journal.evidence, &journal.spec()?.pin)?;
    Ok(journal)
}
pub(super) fn error(e: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Write, format!("释放状态：{e}"))
}
