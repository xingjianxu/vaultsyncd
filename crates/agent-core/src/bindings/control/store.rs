//! S061 SQLite 短事务；无文件系统/网络 I/O，保留历史迁移校验和。
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sqlx::{Row, SqliteConnection};
use vaultsyncd_client_store::entries;
use vaultsyncd_domain::version::Generation;

use super::*;

type Key = [u8; 16];
#[derive(Clone, Default)]
pub(super) struct Control {
    pub paused: bool,
    pub detached: bool,
    pub rules: String,
    pub revision: i64,
    pub mode_pending: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Item {
    pub id: Key,
    parent: Option<Key>,
    name: String,
    pub path: String,
    pub file: bool,
    pub deleted: bool,
    version: Option<Key>,
    revision: Option<i64>,
    local: Option<LocalStamp>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct LocalStamp {
    generation: i64,
    dirty: String,
    residency: String,
    fingerprint: Option<Vec<u8>>,
    identity: Option<Vec<u8>>,
}
impl Item {
    pub fn stamp(&self) -> Result<String, PortError> {
        serde_json::to_string(self).map_err(db)
    }
    fn local_entry(&self) -> Result<Option<LocalEntry>, PortError> {
        self.local
            .as_ref()
            .map(|local| {
                Ok(LocalEntry {
                    entry: EntryId::from_bytes(self.id),
                    generation: Generation::new(u64::try_from(local.generation).map_err(db)?)
                        .map_err(db)?,
                    dirty: match local.dirty.as_str() {
                        "clean" => Dirty::Clean,
                        "dirty" => Dirty::Dirty,
                        "snapshotting" => Dirty::Snapshotting,
                        "awaiting_commit" => Dirty::AwaitingCommit,
                        _ => return Err(rejected("MALFORMED_STATE", "未知 dirty")),
                    },
                    residency: match local.residency.as_str() {
                        "materialized" => Residency::Materialized,
                        "partial" => Residency::Partial,
                        "metadata_only" => Residency::MetadataOnly,
                        _ => return Err(rejected("MALFORMED_STATE", "未知驻留状态")),
                    },
                    fingerprint: local.fingerprint.clone(),
                    file_identity: local.identity.clone(),
                })
            })
            .transpose()
    }
    pub fn full_file(&self, binding: BindingId) -> Result<FullFile, PortError> {
        Ok(FullFile {
            binding,
            entry: EntryId::from_bytes(self.id),
            path: RelativePath::parse(&self.path).map_err(db)?,
            version: self.version.map(VersionId::from_bytes),
            revision: self
                .revision
                .map(|n| Revision::new(u64::try_from(n).map_err(db)?).map_err(db))
                .transpose()?,
            local: self.local_entry()?,
        })
    }
}
pub(super) struct Snapshot {
    pub control: Control,
    pub mode: SyncMode,
    rules_version: String,
    pub items: Vec<Item>,
    pub participation: BTreeMap<Key, String>,
    pub proofs: BTreeMap<Key, String>,
    unlocated: usize,
}
impl Snapshot {
    pub fn ensure_active(&self) -> Result<(), PortError> {
        if self.control.detached {
            return Err(rejected("BINDING_UNBOUND", "绑定已停止管理"));
        }
        if self.control.paused {
            return Err(rejected("BINDING_PAUSED", "绑定已暂停，任务与数据保留"));
        }
        Ok(())
    }
    pub(super) fn progress(&self) -> Result<ModeProgress, PortError> {
        let mut verified = 0;
        let mut remaining = self.unlocated;
        if !self
            .items
            .iter()
            .any(|item| item.parent.is_none() && !item.file && item.name.is_empty())
        {
            remaining += 1;
        }
        for item in &self.items {
            if !item.file || item.deleted || excluded(&self.control.rules, item)? {
                continue;
            }
            if !self.participation.contains_key(&item.id)
                && self.proofs.get(&item.id) == Some(&item.stamp()?)
                && item
                    .local
                    .as_ref()
                    .is_some_and(|local| local.residency == "materialized")
            {
                verified += 1;
            } else {
                remaining += 1;
            }
        }
        Ok(ModeProgress {
            mode: self.mode,
            verified,
            remaining,
            complete: !self.control.detached
                && (self.mode == SyncMode::OnDemand
                    || (!self.control.mode_pending && remaining == 0)),
        })
    }
}
pub(super) struct Store {
    pub database: Arc<Database>,
}
impl Store {
    pub async fn snapshot(&self, binding: BindingId) -> Result<Snapshot, PortError> {
        let mut tx = self.database.pool().begin().await.map_err(db)?;
        let value = snapshot(&mut tx, binding).await?;
        tx.commit().await.map_err(db)?;
        Ok(value)
    }
    pub async fn set_paused(&self, binding: BindingId, paused: bool) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let current = snapshot(&mut tx, binding).await?;
        if current.control.detached {
            return Err(rejected("BINDING_UNBOUND", "不能通过恢复重新绑定"));
        }
        init(&mut tx, binding).await?;
        sqlx::query("UPDATE binding_controls SET paused=?, revision=revision+1 WHERE binding_id=? AND paused<>?")
            .bind(paused).bind(binding.as_bytes().as_slice()).bind(paused).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)
    }
    pub async fn change_mode(&self, binding: BindingId, mode: SyncMode) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let current = snapshot(&mut tx, binding).await?;
        if current.control.detached {
            return Err(rejected("BINDING_UNBOUND", "绑定已停止管理"));
        }
        init(&mut tx, binding).await?;
        // Repeated requests resume the same unfinished task; no new IDs or local content changes.
        if current.mode != mode
            || (mode == SyncMode::Full
                && !current.control.mode_pending
                && !current.progress()?.complete)
        {
            sqlx::query("UPDATE bindings SET mode=? WHERE id=?")
                .bind(mode.as_text())
                .bind(binding.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            sqlx::query(
                "UPDATE binding_controls SET mode_pending=?,revision=revision+1 WHERE binding_id=?",
            )
            .bind(mode == SyncMode::Full)
            .bind(binding.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            sqlx::query("DELETE FROM binding_full_proofs WHERE binding_id=?")
                .bind(binding.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)
    }
    pub async fn change_rules(
        &self,
        binding: BindingId,
        version: &str,
        rules: &str,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let current = snapshot(&mut tx, binding).await?;
        if current.control.detached {
            return Err(rejected("BINDING_UNBOUND", "绑定已停止管理"));
        }
        if current.rules_version == version && current.control.rules == rules {
            return Ok(());
        }
        if current.rules_version == version {
            return Err(rejected("RULES_VERSION_REUSED", "相同版本不能对应不同规则"));
        }
        let reused: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM binding_rules_history WHERE binding_id=? AND version=?",
        )
        .bind(binding.as_bytes().as_slice())
        .bind(version)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if reused != 0 {
            return Err(rejected("RULES_VERSION_REUSED", "历史规则版本不能再次使用"));
        }
        init(&mut tx, binding).await?;
        for rule_version in [&current.rules_version, &version.to_owned()] {
            sqlx::query(
                "INSERT OR IGNORE INTO binding_rules_history(binding_id,version) VALUES (?,?)",
            )
            .bind(binding.as_bytes().as_slice())
            .bind(rule_version)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        for item in &current.items {
            if item.path.is_empty() {
                continue;
            }
            let now = excluded(rules, item)?;
            let was = excluded(&current.control.rules, item)?
                || current.participation.contains_key(&item.id);
            if now || was {
                sqlx::query("INSERT INTO binding_participation(binding_id,entry_id,state) VALUES (?,?,?) ON CONFLICT(binding_id,entry_id) DO UPDATE SET state=excluded.state")
                    .bind(binding.as_bytes().as_slice()).bind(item.id.as_slice()).bind(if now {"excluded"} else {"reconcile_required"}).execute(&mut *tx).await.map_err(db)?;
            }
        }
        sqlx::query("UPDATE bindings SET rules_version=? WHERE id=?")
            .bind(version)
            .bind(binding.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("UPDATE binding_controls SET rules=?,revision=revision+1,mode_pending=CASE WHEN (SELECT mode FROM bindings WHERE id=?)='full' THEN 1 ELSE 0 END WHERE binding_id=?")
            .bind(rules).bind(binding.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)
    }
    pub async fn reconciled(
        &self,
        binding: BindingId,
        entry: EntryId,
        version: &str,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let current = snapshot(&mut tx, binding).await?;
        current.ensure_active()?;
        if current.rules_version != version {
            return Err(rejected("RULES_CHANGED", "规则已变化，必须重新协调"));
        }
        let item = current
            .items
            .iter()
            .find(|item| item.id == *entry.as_bytes())
            .ok_or_else(|| rejected("RECONCILE_REQUIRED", "缺少目标身份/位置"))?;
        if excluded(&current.control.rules, item)? {
            return Err(rejected("EXCLUDED", "仍在排除规则内"));
        }
        sqlx::query("DELETE FROM binding_participation WHERE binding_id=? AND entry_id=?")
            .bind(binding.as_bytes().as_slice())
            .bind(entry.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)
    }
    pub async fn unbind_report(&self, binding: BindingId) -> Result<UnbindReport, PortError> {
        let mut tx = self.database.pool().begin().await.map_err(db)?;
        let report = report(&mut tx, binding).await?;
        tx.commit().await.map_err(db)?;
        Ok(report)
    }
    pub async fn unbind(&self, binding: BindingId) -> Result<UnbindReport, PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let result = report(&mut tx, binding).await?;
        init(&mut tx, binding).await?;
        sqlx::query("UPDATE binding_controls SET detached=1,paused=1,revision=revision+1 WHERE binding_id=? AND detached=0")
            .bind(binding.as_bytes().as_slice()).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
    pub async fn progress(&self, binding: BindingId) -> Result<ModeProgress, PortError> {
        self.snapshot(binding).await?.progress()
    }
    pub async fn certify(
        &self,
        binding: BindingId,
        before: &Snapshot,
        item: &Item,
        proof: &LocalEntry,
    ) -> Result<(), PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let after = snapshot(&mut tx, binding).await?;
        after.ensure_active()?;
        if after.control.revision != before.control.revision || after.mode != SyncMode::Full {
            return Err(rejected(
                "BINDING_CHANGED",
                "下载期间绑定/规则/模式变化，保留结果重新协调",
            ));
        }
        let current = after
            .items
            .iter()
            .find(|value| value.id == item.id)
            .ok_or_else(|| rejected("REMOTE_CHANGED", "条目已消失"))?;
        if current.parent != item.parent
            || current.path != item.path
            || current.deleted != item.deleted
            || current.version != item.version
            || current.revision != item.revision
        {
            return Err(rejected("REMOTE_CHANGED", "固定目标已变化"));
        }
        if current.local_entry()?.as_ref() != Some(proof) {
            return Err(rejected(
                "LOCAL_CHANGED",
                "核对后本地代际/驻留/身份发生变化",
            ));
        }
        // Even a complete dirty file cannot stand in for an unapplied remote version.
        // Reconcile the conflict while retaining its bytes before declaring full complete.
        if let Some(revision) = item.revision {
            let baseline = entries::baseline_entry(&mut tx, binding, proof.entry)
                .await
                .map_err(db)?;
            if baseline.is_none_or(|base| {
                base.deleted
                    || base.revision.get() != revision as u64
                    || base.content_version.map(|id| *id.as_bytes()) != item.version
            }) {
                return Err(rejected(
                    "REMOTE_NOT_APPLIED",
                    "仅下载或旧clean不能完成模式任务",
                ));
            }
        }
        sqlx::query("INSERT INTO binding_full_proofs(binding_id,entry_id,stamp) VALUES (?,?,?) ON CONFLICT(binding_id,entry_id) DO UPDATE SET stamp=excluded.stamp")
            .bind(binding.as_bytes().as_slice()).bind(item.id.as_slice()).bind(current.stamp()?).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)
    }
    pub async fn finish_mode(
        &self,
        binding: BindingId,
        revision: i64,
    ) -> Result<ModeProgress, PortError> {
        let mut tx = self.database.begin_write().await.map_err(db)?;
        let current = snapshot(&mut tx, binding).await?;
        current.ensure_active()?;
        if current.control.revision != revision || current.mode != SyncMode::Full {
            return Err(rejected("BINDING_CHANGED", "模式任务目标已变化"));
        }
        init(&mut tx, binding).await?;
        let mut result = current.progress()?;
        if result.remaining == 0 {
            sqlx::query("UPDATE binding_controls SET mode_pending=0 WHERE binding_id=?")
                .bind(binding.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            result.complete = true;
        }
        tx.commit().await.map_err(db)?;
        Ok(result)
    }
}
async fn init(conn: &mut SqliteConnection, binding: BindingId) -> Result<(), PortError> {
    sqlx::query("INSERT OR IGNORE INTO binding_controls(binding_id) VALUES (?)")
        .bind(binding.as_bytes().as_slice())
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(())
}
async fn report(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<UnbindReport, PortError> {
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM bindings WHERE id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_one(&mut *conn)
        .await
        .map_err(db)?;
    if exists != 1 {
        return Err(rejected("BINDING_MISSING", "绑定不存在"));
    }
    let rows = sqlx::query(
        "SELECT entry_id,dirty,residency FROM local_entries WHERE binding_id=? ORDER BY entry_id",
    )
    .bind(binding.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(db)?;
    let mut value = UnbindReport {
        unconfirmed: Vec::new(),
        incomplete: Vec::new(),
        offer_export_recovery: false,
    };
    for row in rows {
        let id = EntryId::from_bytes(key(row.try_get("entry_id").map_err(db)?)?);
        if row.try_get::<String, _>("dirty").map_err(db)? != "clean" {
            value.unconfirmed.push(id);
        }
        if row.try_get::<String, _>("residency").map_err(db)? != "materialized" {
            value.incomplete.push(id);
        }
    }
    value.offer_export_recovery = !value.unconfirmed.is_empty();
    Ok(value)
}
pub(super) async fn snapshot(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<Snapshot, PortError> {
    let row = sqlx::query("SELECT mode,rules_version FROM bindings WHERE id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?
        .ok_or_else(|| rejected("BINDING_MISSING", "绑定不存在"))?;
    let mode = SyncMode::from_text(row.try_get::<&str, _>("mode").map_err(db)?)
        .ok_or_else(|| rejected("MALFORMED_STATE", "模式无效"))?;
    let rules_version = row.try_get("rules_version").map_err(db)?;
    let control = sqlx::query("SELECT paused,detached,rules,revision,mode_pending FROM binding_controls WHERE binding_id=?").bind(binding.as_bytes().as_slice()).fetch_optional(&mut *conn).await.map_err(db)?.map(|r| Ok::<_,PortError>(Control {
        paused: r.try_get("paused").map_err(db)?, detached: r.try_get("detached").map_err(db)?, rules: r.try_get("rules").map_err(db)?, revision: r.try_get("revision").map_err(db)?, mode_pending: r.try_get("mode_pending").map_err(db)?,
    })).transpose()?.unwrap_or_default();
    let rows = sqlx::query("SELECT * FROM baseline_entries WHERE binding_id=? AND entry_id NOT IN (SELECT entry_id FROM remote_entries WHERE binding_id=?) UNION ALL SELECT * FROM remote_entries WHERE binding_id=?")
        .bind(binding.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).fetch_all(&mut *conn).await.map_err(db)?;
    let mut items = BTreeMap::new();
    for row in rows {
        let id = key(row.try_get("entry_id").map_err(db)?)?;
        let parent = row
            .try_get::<Option<Vec<u8>>, _>("parent_entry_id")
            .map_err(db)?
            .map(key)
            .transpose()?;
        let local = sqlx::query("SELECT * FROM local_entries WHERE binding_id=? AND entry_id=?")
            .bind(binding.as_bytes().as_slice())
            .bind(id.as_slice())
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?
            .map(|r| {
                Ok::<_, PortError>(LocalStamp {
                    generation: r.try_get("generation").map_err(db)?,
                    dirty: r.try_get("dirty").map_err(db)?,
                    residency: r.try_get("residency").map_err(db)?,
                    fingerprint: r.try_get("fingerprint").map_err(db)?,
                    identity: r.try_get("file_identity").map_err(db)?,
                })
            })
            .transpose()?;
        items.insert(
            id,
            Item {
                id,
                parent,
                name: row.try_get("name").map_err(db)?,
                path: String::new(),
                file: row.try_get::<&str, _>("kind").map_err(db)? == "file",
                deleted: row.try_get("deleted").map_err(db)?,
                version: row
                    .try_get::<Option<Vec<u8>>, _>("content_version_id")
                    .map_err(db)?
                    .map(key)
                    .transpose()?,
                revision: Some(row.try_get("revision").map_err(db)?),
                local,
            },
        );
    }
    let mut resolved = Vec::new();
    for item in items.values() {
        let mut value = item.clone();
        let mut current = item;
        let mut seen = BTreeSet::new();
        let mut names = Vec::new();
        while let Some(parent) = current.parent {
            if !seen.insert(current.id) {
                return Err(rejected("MALFORMED_STATE", "namespace父链成环"));
            }
            names.push(current.name.clone());
            current = items
                .get(&parent)
                .ok_or_else(|| rejected("RECONCILE_REQUIRED", "namespace缺少父目录，不能猜路径"))?;
            if current.file {
                return Err(rejected("MALFORMED_STATE", "父条目不是目录"));
            }
        }
        names.reverse();
        value.path = names.join("/");
        if !value.path.is_empty() {
            RelativePath::parse(&value.path).map_err(db)?;
        }
        resolved.push(value);
    }
    let mut participation = BTreeMap::new();
    for row in sqlx::query("SELECT entry_id,state FROM binding_participation WHERE binding_id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?
    {
        participation.insert(
            key(row.try_get("entry_id").map_err(db)?)?,
            row.try_get("state").map_err(db)?,
        );
    }
    let mut proofs = BTreeMap::new();
    for row in sqlx::query("SELECT entry_id,stamp FROM binding_full_proofs WHERE binding_id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?
    {
        proofs.insert(
            key(row.try_get("entry_id").map_err(db)?)?,
            row.try_get("stamp").map_err(db)?,
        );
    }
    let unlocated: i64 = sqlx::query_scalar("SELECT count(*) FROM local_entries WHERE binding_id=? AND entry_id NOT IN (SELECT entry_id FROM baseline_entries WHERE binding_id=? UNION SELECT entry_id FROM remote_entries WHERE binding_id=?)")
        .bind(binding.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).fetch_one(&mut *conn).await.map_err(db)?;
    Ok(Snapshot {
        control,
        mode,
        rules_version,
        items: resolved,
        participation,
        proofs,
        unlocated: usize::try_from(unlocated).map_err(db)?,
    })
}
fn key(bytes: Vec<u8>) -> Result<Key, PortError> {
    bytes
        .try_into()
        .map_err(|_| rejected("MALFORMED_STATE", "身份长度无效"))
}
