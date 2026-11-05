//! 测试共享的临时目录与种子数据（仅 `cfg(test)` 编译）。
//!
//! 每个测试一个独立目录与独立 SQLite 文件，`Drop` 时连同 WAL 与迁移前备份一起删除。
//! 种子数据只经本卡已建的 `users` 表与 [`crate::roots::insert_root`] 写入，不绕过被测代码；
//! 账号本身还没有 typed API（由账号卡 S042 提供），所以测试直接写那一张表。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::{Sqlite, Transaction};
use vaultsyncd_domain::ids::{DeviceId, EntryId, ID_LEN, OpId, RootId, TxId, UserId};
use vaultsyncd_domain::version::ServerEpoch;

use crate::db::Database;
use crate::entries::{self, EntryKind, NewEntry};
use crate::roots::{self, NewRoot};

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

pub(crate) fn root(marker: u8) -> RootId {
    RootId::from_bytes([marker; ID_LEN])
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

/// 建一个测试账号并提交（调用方随后注册 Root）。
pub(crate) async fn seed_owner(database: &Database, marker: u8) -> UserId {
    let id = user(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    insert_user(&mut tx, id).await;
    tx.commit().await.expect("提交测试账号");
    id
}

/// 在同一个事务里注册 Root 与它的合成根条目，返回 `(RootId, 根条目 EntryId)`。
pub(crate) async fn register_root(
    database: &Database,
    owner: UserId,
    marker: u8,
) -> (RootId, EntryId) {
    let root = root(marker);
    let root_entry = entry(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    roots::insert_root(
        &mut tx,
        &NewRoot {
            id: root,
            entry: root_entry,
            owner,
            absolute_path: format!("/srv/vaultsyncd-{marker:02x}"),
            volume_identity: vec![marker, 0x01],
            root_file_identity: vec![marker, 0x02],
            epoch: ServerEpoch::ZERO,
        },
    )
    .await
    .expect("注册测试 Root");
    tx.commit().await.expect("提交测试 Root");
    (root, root_entry)
}

/// 在给定父目录下写一个文件条目并提交，返回其身份。
///
/// 内容版本不在新建时指定：版本必须由 `versions::record_version` 在条目存在后写入
/// （`NewEntry::content_version` 只接受已记录到该条目的版本）。
pub(crate) async fn seed_file(
    database: &Database,
    owner: UserId,
    root: RootId,
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
            root,
            parent,
            name: name.to_owned(),
            name_key: name.to_owned(),
            kind: EntryKind::File,
            revision: vaultsyncd_domain::version::Revision::new(1).expect("修订号"),
            content_version: None,
        },
    )
    .await
    .expect("写测试文件条目");
    tx.commit().await.expect("提交测试文件条目");
    id
}
