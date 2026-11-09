//! 恢复包：固定快照流式导出，清单最后发布，不修改同步状态或清除dirty。
use crate::status::error;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{Column, Row, TypeInfo};
use std::sync::Arc;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_transfer::{
    hash::{ContentHasher, to_hex},
    ports::{PortError, PortFuture, SnapshotSource},
};
/// 内容来源标识；本地条目与上传固定快照必须分别保全。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// 稳定EntryId或OpId的十六进制，不采用用户路径作为目的文件名。
    pub id: String,
    /// 当前内容current、固定上传snapshot或平台发现candidate。
    pub kind: String,
    /// 固定快照身份或本地代际，由源适配器核对，不执行该文本。
    pub identity: String,
    /// 固定上传内容已知摘要。
    pub digest: Option<String>,
    /// 固定上传内容已知大小。
    pub size: Option<u64>,
}
/// 平台源：固定快照/原生安全文件句柄，禁止回源下载替代未上传字节。
pub trait ExportSource: Send + Sync {
    /// 补充尚未入库的本地文件、冲突候选/临时上传快照；调用方需扫描完成或明确报错。
    fn discover(&self, binding: BindingId) -> PortFuture<'_, Result<Vec<Target>, PortError>>;
    /// 返回不可变快照，核对target身份/generation；未知/部分且不能完整恢复必须失败。
    fn open<'a>(
        &'a self,
        target: &'a Target,
    ) -> PortFuture<'a, Result<Box<dyn SnapshotSource>, PortError>>;
    /// 导出后重新核对活动来源仍对应target，变化则失败且不发布成功清单。
    fn verify<'a>(&'a self, target: &'a Target) -> PortFuture<'a, Result<(), PortError>>;
}
/// 已创建的新恢复目录的连续安全句柄。所有名字仅单分量且由核心生成。
/// 实现必须保持目录身份，不跟随symlink/reparse，create_new且禁止覆盖，包括并发替换。
pub trait ExportDirectory: Send + Sync {
    /// 创建新文件，任何同名对象（含symlink）都明确拒绝。
    fn create<'a>(&'a self, name: &'a str) -> PortFuture<'a, Result<(), PortError>>;
    /// 按顺序追加并持久化；不能重新按不受保护路径打开。
    fn append<'a>(
        &'a self,
        name: &'a str,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>>;
    /// 从同一受保护文件读取以验证输出，不接受仅源端摘要。
    fn read<'a>(
        &'a self,
        name: &'a str,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>>;
    /// fsync所有文件与目录后发布；manifest.json是最后一个文件，失败目录不能冒充成功包。
    fn finish(&self) -> PortFuture<'_, Result<(), PortError>>;
}
/// 目的端：指定的绝对目录必须不存在，其祖先需以安全句柄遍历，且与同步/缓存目录不重叠。
pub trait ExportDestination: Send + Sync {
    /// 原子创建新目录；失败不清理或覆盖用户既有对象。
    fn create_new<'a>(
        &'a self,
        destination: &'a str,
    ) -> PortFuture<'a, Result<Box<dyn ExportDirectory>, PortError>>;
}
/// 可独立验证的恢复文件记录。
#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    /// 单分量文件名。
    pub name: String,
    /// 字节数。
    pub size: u64,
    /// SHA-256。
    pub sha256: String,
    /// 内容来源，state.json记录为None。
    pub source: Option<Target>,
}
/// 成功清单；版本为恢复包格式，不是wire DTO。
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// 恢复包格式版本。
    pub format: u32,
    /// 原绑定身份。
    pub binding: String,
    /// 文件清单。
    pub files: Vec<ManifestFile>,
}
/// 组装恢复导出；源与目的平台适配器由应用入口提供。
pub struct ExportRecovery {
    db: Arc<Database>,
}
impl ExportRecovery {
    /// 仅持有SQLite，不直接访问平台文件。
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
    /// 全部内容与状态写完并核验后才发布manifest；失败保留部分现场供用户查看但不报告完成。
    pub async fn run(
        &self,
        binding: BindingId,
        destination: &str,
        source: &dyn ExportSource,
        sink: &dyn ExportDestination,
    ) -> Result<Manifest, PortError> {
        let (state, mut targets) = catalog(&self.db, binding).await?;
        let discovered = source.discover(binding).await?;
        targets.extend(discovered.clone());
        let mut seen = std::collections::HashSet::new();
        for t in &targets {
            if !["current", "snapshot", "candidate"].contains(&t.kind.as_str())
                || t.id.len() != 32
                || !t.id.bytes().all(|c| c.is_ascii_hexdigit())
                || !seen.insert((t.kind.clone(), t.id.clone()))
            {
                return Err(PortError::protocol("恢复来源身份重复或无效"));
            }
        }
        let directory = sink.create_new(destination).await?;
        let mut manifest = Manifest {
            format: 1,
            binding: binding.to_string(),
            files: vec![],
        };
        let bytes = serde_json::to_vec_pretty(&state).map_err(error)?;
        write(&*directory, "state.json", &bytes).await?;
        manifest.files.push(ManifestFile {
            name: "state.json".into(),
            size: bytes.len() as u64,
            sha256: to_hex(&vaultsyncd_transfer::hash::digest_of(&bytes)),
            source: None,
        });
        for t in &targets {
            let snapshot = source.open(t).await?;
            let size = snapshot.total_size();
            if t.kind == "snapshot" && snapshot.identity() != t.identity {
                return Err(super::changed());
            }
            if t.size.is_some_and(|n| n != size) {
                return Err(super::changed());
            }
            let name = format!("{}-{}.bin", t.kind, t.id);
            directory.create(&name).await?;
            let mut hash = ContentHasher::new();
            let mut offset = 0;
            while offset < size {
                let length = (size - offset).min(256 * 1024);
                let bytes = snapshot.read(offset, length).await?;
                if bytes.len() as u64 != length {
                    return Err(PortError::protocol("恢复快照短读"));
                }
                hash.update(&bytes);
                directory.append(&name, &bytes).await?;
                offset += length;
            }
            let digest = to_hex(&hash.finish());
            if t.digest.as_ref().is_some_and(|d| d != &digest) {
                return Err(super::changed());
            }
            verify_file(&*directory, &name, size, &digest).await?;
            source.verify(t).await?;
            manifest.files.push(ManifestFile {
                name,
                size,
                sha256: digest,
                source: Some(t.clone()),
            });
        }
        // 数据库中的generation/队列/固定快照变化也使旧目录不能被标为完整。
        if catalog(&self.db, binding).await?.0 != state {
            return Err(super::changed());
        }
        for target in &targets {
            source.verify(target).await?;
        }
        if source.discover(binding).await? != discovered {
            return Err(super::changed());
        }
        verify_file(
            &*directory,
            "state.json",
            bytes.len() as u64,
            &manifest.files[0].sha256,
        )
        .await?;
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(error)?;
        write(&*directory, "manifest.json", &manifest_bytes).await?;
        verify_file(
            &*directory,
            "manifest.json",
            manifest_bytes.len() as u64,
            &to_hex(&vaultsyncd_transfer::hash::digest_of(&manifest_bytes)),
        )
        .await?;
        directory.finish().await?;
        validate(&*directory, &manifest).await?;
        Ok(manifest)
    }
}
async fn write(d: &dyn ExportDirectory, name: &str, bytes: &[u8]) -> Result<(), PortError> {
    d.create(name).await?;
    d.append(name, bytes).await
}
/// 独立重核清单中的文件大小、摘要和单分量名字（目录句柄实现必须安全）。
pub async fn validate(
    directory: &dyn ExportDirectory,
    manifest: &Manifest,
) -> Result<(), PortError> {
    if manifest.format != 1
        || BindingId::from_hex(&manifest.binding).is_err()
        || manifest
            .files
            .iter()
            .filter(|f| f.name == "state.json" && f.source.is_none())
            .count()
            != 1
    {
        return Err(PortError::protocol("未知恢复包版本"));
    }
    let mut names = std::collections::HashSet::new();
    for file in &manifest.files {
        vaultsyncd_fs_common::name::validate_name(&file.name).map_err(error)?;
        if file.name == "manifest.json"
            || file.sha256.len() != 64
            || !file.sha256.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(PortError::protocol("非法恢复清单"));
        }
        if let Some(t) = &file.source {
            if t.size.is_some_and(|size| size != file.size)
                || t.digest.as_ref().is_some_and(|hash| hash != &file.sha256)
            {
                return Err(PortError::protocol("固定来源与清单内容事实不符"));
            }
            if !["current", "snapshot", "candidate"].contains(&t.kind.as_str())
                || t.id.len() != 32
                || !t.id.bytes().all(|c| c.is_ascii_hexdigit())
                || file.name != format!("{}-{}.bin", t.kind, t.id)
            {
                return Err(PortError::protocol("内容来源与文件名不符"));
            }
        } else if file.name != "state.json" {
            return Err(PortError::protocol("无来源内容"));
        }
        if file.name.is_empty()
            || file.name.contains(['/', '\\', ':'])
            || file.name == "."
            || file.name == ".."
            || !names.insert(&file.name)
        {
            return Err(PortError::protocol("无效恢复文件名"));
        }
        verify_file(directory, &file.name, file.size, &file.sha256).await?;
        if file.name == "state.json" {
            let bytes = directory.read(&file.name, 0, file.size).await?;
            let state: Value = serde_json::from_slice(&bytes).map_err(error)?;
            if state["binding"]["id"]
                .as_str()
                .map(|s| s.to_ascii_lowercase())
                != Some(manifest.binding.clone())
            {
                return Err(PortError::protocol("恢复状态与清单绑定不符"));
            }
        }
    }
    Ok(())
}
async fn verify_file(
    d: &dyn ExportDirectory,
    name: &str,
    size: u64,
    digest: &str,
) -> Result<(), PortError> {
    let mut hash = ContentHasher::new();
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(256 * 1024);
        let bytes = d.read(name, offset, length).await?;
        if bytes.len() as u64 != length {
            return Err(PortError::protocol("导出文件短读"));
        }
        hash.update(&bytes);
        offset += length;
    }
    if !d.read(name, size, 1).await?.is_empty() || to_hex(&hash.finish()) != digest {
        return Err(PortError::protocol("恢复文件长度或摘要不符"));
    }
    Ok(())
}
async fn catalog(db: &Database, binding: BindingId) -> Result<(Value, Vec<Target>), PortError> {
    let mut tx = db.pool().begin().await.map_err(error)?;
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM bindings WHERE id=?")
        .bind(binding.as_bytes().as_slice())
        .fetch_one(&mut *tx)
        .await
        .map_err(error)?;
    if exists != 1 {
        return Err(PortError::protocol("绑定不存在"));
    }
    let mut state = Map::new();
    let mut targets = vec![];
    let binding_json:String=sqlx::query_scalar("SELECT json_object('id',hex(id),'server_id',hex(server_id),'device_id',hex(device_id),'vault_id',hex(vault_id),'server_epoch',server_epoch,'mode',mode,'received_seq',received_seq,'applied_seq',applied_seq,'rules_version',rules_version) FROM bindings WHERE id=?").bind(binding.as_bytes().as_slice()).fetch_one(&mut *tx).await.map_err(error)?;
    state.insert(
        "binding".into(),
        serde_json::from_str(&binding_json).map_err(error)?,
    );
    for table in [
        "baseline_entries",
        "local_entries",
        "remote_entries",
        "pending_ops",
        "upload_resumes",
        "local_intents",
        "binding_resyncs",
        "controlled_applies",
        "controlled_releases",
    ] {
        let rows = sqlx::query(&format!(
            "SELECT * FROM {table} WHERE binding_id=? ORDER BY rowid"
        ))
        .bind(binding.as_bytes().as_slice())
        .fetch_all(&mut *tx)
        .await
        .map_err(error)?;
        let mut values = vec![];
        for row in rows {
            let mut object = Map::new();
            for column in row.columns() {
                let name = column.name();
                if name == "session_id" {
                    continue;
                }
                let value = match column.type_info().name() {
                    "BLOB" => row
                        .try_get::<Option<Vec<u8>>, _>(name)
                        .map_err(error)?
                        .map(|v| Value::String(hex(&v))),
                    "INTEGER" => row
                        .try_get::<Option<i64>, _>(name)
                        .map_err(error)?
                        .map(Value::from),
                    _ => row
                        .try_get::<Option<String>, _>(name)
                        .map_err(error)?
                        .map(Value::String),
                }
                .unwrap_or(Value::Null);
                object.insert(name.into(), value);
            }
            let directory = table == "local_entries"
                && state
                    .get("baseline_entries")
                    .and_then(Value::as_array)
                    .is_some_and(|rows| {
                        rows.iter().any(|base| {
                            base["entry_id"] == object["entry_id"] && base["kind"] == "directory"
                        })
                    });
            if table == "local_entries"
                && !directory
                && (object["residency"] != "metadata_only" || object["dirty"] != "clean")
            {
                targets.push(Target {
                    id: object["entry_id"]
                        .as_str()
                        .ok_or_else(|| PortError::protocol("缺少内容身份"))?
                        .into(),
                    kind: "current".into(),
                    identity: object["generation"].to_string(),
                    digest: None,
                    size: None,
                });
            }
            if table == "upload_resumes" {
                targets.push(Target {
                    id: object["op_id"]
                        .as_str()
                        .ok_or_else(|| PortError::protocol("缺少快照身份"))?
                        .into(),
                    kind: "snapshot".into(),
                    identity: object["snapshot_identity"]
                        .as_str()
                        .ok_or_else(|| PortError::protocol("缺少快照标识"))?
                        .into(),
                    digest: object["digest"].as_str().map(String::from),
                    size: object["total_size"].as_u64(),
                });
            }
            values.push(Value::Object(object));
        }
        state.insert(table.into(), Value::Array(values));
    }
    let quarantined:Vec<Vec<u8>>=sqlx::query_scalar("SELECT q.op_id FROM resync_quarantined_ops q JOIN binding_resyncs r USING(resync_id) WHERE r.binding_id=? ORDER BY q.op_id").bind(binding.as_bytes().as_slice()).fetch_all(&mut *tx).await.map_err(error)?;
    state.insert(
        "quarantined_ops".into(),
        Value::Array(quarantined.iter().map(|v| Value::String(hex(v))).collect()),
    );
    tx.commit().await.map_err(error)?;
    Ok((Value::Object(state), targets))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(test)]
mod tests;
