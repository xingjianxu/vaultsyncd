//! 测试共享的临时目录与种子数据（仅 `cfg(test)` 编译）。
//!
//! 每个测试一个独立目录与独立 SQLite 文件，`Drop` 时连同 WAL 与迁移前备份一起删除。
//! 种子数据只经本卡已建的 `users` 表与 [`crate::vaults::insert_vault`] 写入，不绕过被测代码；
//! 账号本身还没有 typed API（由账号卡 S042 提供），所以测试直接写那一张表。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::{Sqlite, Transaction};
use vaultsyncd_domain::ids::{
    BackupLeaseId, DeviceId, EntryId, ID_LEN, IntentId, ObjectLeaseId, OpId, SessionId, TxId,
    UploadId, UserId, VaultId,
};
use vaultsyncd_domain::version::{ServerEpoch, UtcTimestamp};

use crate::db::Database;
use crate::entries::{self, EntryKind, NewEntry};
use crate::vaults::{self, NewVault};
use crate::versions::{self, NewObject, ObjectHash};

/// 测试独占的临时目录；`Drop` 时整个删除。
pub(crate) struct TestDir {
    pub(crate) path: PathBuf,
}

impl TestDir {
    pub(crate) fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s027-{}-{label}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    pub(crate) fn database(&self) -> PathBuf {
        self.path.join("server.sqlite")
    }

    /// 按给定文件写出一个迁移目录，供 `Migrator::new` 解析（构造“上一版 schema”的旧库）。
    pub(crate) fn write_migrations(&self, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.path.join("migrations");
        std::fs::create_dir_all(&dir).expect("创建 migrations 目录");
        for (name, sql) in files {
            std::fs::write(dir.join(name), sql).expect("写入迁移文件");
        }
        dir
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 打开（必要时建库并迁移到当前 schema）测试库。
pub(crate) async fn open_database(dir: &TestDir) -> Database {
    Database::open(dir.database()).await.expect("打开测试库")
}

/// 固定字节的身份，便于在断言里看出是哪一个对象。
pub(crate) fn user(marker: u8) -> UserId {
    UserId::from_bytes([marker; ID_LEN])
}

pub(crate) fn vault(marker: u8) -> VaultId {
    VaultId::from_bytes([marker; ID_LEN])
}

pub(crate) fn entry(marker: u8) -> EntryId {
    EntryId::from_bytes([marker; ID_LEN])
}

pub(crate) fn tx(marker: u8) -> TxId {
    TxId::from_bytes([marker; ID_LEN])
}

pub(crate) fn device(marker: u8) -> DeviceId {
    DeviceId::from_bytes([marker; ID_LEN])
}

pub(crate) fn op(marker: u8) -> OpId {
    OpId::from_bytes([marker; ID_LEN])
}

pub(crate) fn session(marker: u8) -> SessionId {
    SessionId::from_bytes([marker; ID_LEN])
}

pub(crate) fn upload(marker: u8) -> UploadId {
    UploadId::from_bytes([marker; ID_LEN])
}

pub(crate) fn intent(marker: u8) -> IntentId {
    IntentId::from_bytes([marker; ID_LEN])
}

pub(crate) fn object_lease(marker: u8) -> ObjectLeaseId {
    ObjectLeaseId::from_bytes([marker; ID_LEN])
}

pub(crate) fn backup_lease(marker: u8) -> BackupLeaseId {
    BackupLeaseId::from_bytes([marker; ID_LEN])
}

/// 固定字节的内容哈希：便于在断言里看出是哪一个对象/块。
pub(crate) fn content_hash(marker: u8) -> ObjectHash {
    [marker; 32]
}

/// 固定 UTC 时间戳（秒）。
pub(crate) fn at(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(seconds).expect("固定时间戳必须合法")
}

/// 写一个测试账号；`login` 用身份文本保证唯一。
pub(crate) async fn insert_user(tx: &mut Transaction<'_, Sqlite>, id: UserId) {
    sqlx::query(
        "INSERT INTO users (id, login, password_hash, status) \
         VALUES (?1, ?2, 'test-only-hash', 'active')",
    )
    .bind(id.as_bytes().as_slice())
    .bind(id.to_hex())
    .execute(&mut **tx)
    .await
    .expect("写测试账号");
}

/// 建一个测试账号并提交（调用方随后注册 Vault）。
pub(crate) async fn seed_owner(database: &Database, marker: u8) -> UserId {
    let id = user(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    insert_user(&mut tx, id).await;
    tx.commit().await.expect("提交测试账号");
    id
}

/// 在同一个事务里注册 Vault 与它的合成根条目，返回 `(VaultId, 根条目 EntryId)`。
pub(crate) async fn register_vault(
    database: &Database,
    owner: UserId,
    marker: u8,
) -> (VaultId, EntryId) {
    let vault = vault(marker);
    let vault_entry = entry(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    vaults::insert_vault(
        &mut tx,
        &NewVault {
            id: vault,
            entry: vault_entry,
            owner,
            absolute_path: format!("/srv/vaultsyncd-{marker:02x}"),
            volume_identity: vec![marker, 0x01],
            vault_file_identity: vec![marker, 0x02],
            epoch: ServerEpoch::ZERO,
        },
    )
    .await
    .expect("注册测试 Vault");
    tx.commit().await.expect("提交测试 Vault");
    (vault, vault_entry)
}

/// 在只有 0001—0012 的旧库里注册 Vault：那一版表与列名是 `roots`/`root_id`/`root_file_identity`，
/// S140 的迁移 0016 才改成 Vault。升级类测试要构造“旧服务端建出的库”，必须写旧名字。
pub(crate) async fn register_vault_at_v12(
    database: &Database,
    owner: UserId,
    marker: u8,
) -> (VaultId, EntryId) {
    let id = vault(marker);
    let synthetic = entry(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    sqlx::query(
        "INSERT INTO roots(id,owner_id,absolute_path,volume_identity,root_file_identity,health,seq,epoch) \
         VALUES (?1,?2,?3,?4,?5,'healthy',0,0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .bind(format!("/srv/vaultsyncd-{marker:02x}"))
    .bind(vec![marker, 0x01])
    .bind(vec![marker, 0x02])
    .execute(&mut *tx)
    .await
    .expect("写 0001—0012 版测试 Vault");
    sqlx::query(
        "INSERT INTO entries(id,root_id,parent_id,name,name_key,kind,revision) \
         VALUES (?1,?2,NULL,'','','directory',0)",
    )
    .bind(synthetic.as_bytes().as_slice())
    .bind(id.as_bytes().as_slice())
    .execute(&mut *tx)
    .await
    .expect("写 0001—0012 版合成根条目");
    tx.commit().await.expect("提交 0001—0012 版测试 Vault");
    (id, synthetic)
}

/// 在旧库里追加一条事务批次并推进 `roots.seq`；列名仍是 `root_id`（S140 前）。
pub(crate) async fn append_change_at_v12(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: UserId,
    id: VaultId,
    tx_id: TxId,
    events_json: &str,
    at: UtcTimestamp,
) -> i64 {
    let seq: i64 = sqlx::query_scalar(
        "UPDATE roots SET seq = seq + 1 WHERE id = ?1 AND owner_id = ?2 RETURNING seq",
    )
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_one(&mut **tx)
    .await
    .expect("推进旧版 roots.seq");
    sqlx::query(
        "INSERT INTO changes(root_id,seq,tx_id,events_json,created_at) VALUES (?1,?2,?3,?4,?5)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(seq)
    .bind(tx_id.as_bytes().as_slice())
    .bind(events_json)
    .bind(at.as_unix_seconds())
    .execute(&mut **tx)
    .await
    .expect("写旧版 changes 批次");
    seq
}

/// 登记一个内容对象并提交，返回它的内容哈希。
///
/// 走 [`versions::record_object`]，不绕过被测代码：租约与版本都必须引用**已登记**的对象。
pub(crate) async fn seed_object(
    database: &Database,
    owner: UserId,
    vault: VaultId,
    marker: u8,
    size: u64,
) -> ObjectHash {
    let hash = content_hash(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    versions::record_object(
        &mut tx,
        owner,
        &NewObject {
            vault,
            hash,
            size,
            storage_path: format!("/srv/objects/{marker:02x}"),
            verified_at: at(1_700_000_000),
        },
    )
    .await
    .expect("登记测试对象");
    tx.commit().await.expect("提交测试对象");
    hash
}

/// 在给定父目录下写一个文件条目并提交，返回其身份。
///
/// 内容版本不在新建时指定：版本必须由 `versions::record_version` 在条目存在后写入
/// （`NewEntry::content_version` 只接受已记录到该条目的版本）。
pub(crate) async fn seed_file(
    database: &Database,
    owner: UserId,
    vault: VaultId,
    parent: EntryId,
    marker: u8,
    name: &str,
) -> EntryId {
    let id = entry(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    entries::insert_entry(
        &mut tx,
        owner,
        &NewEntry {
            id,
            vault,
            parent,
            name: name.to_owned(),
            name_key: name.to_owned(),
            kind: EntryKind::File,
            revision: vaultsyncd_domain::version::Revision::new(1).expect("修订号"),
            content_version: None,
        },
        at(1_700_000_100),
    )
    .await
    .expect("写测试文件条目");
    tx.commit().await.expect("提交测试文件条目");
    id
}
