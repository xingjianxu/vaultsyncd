//! 测试共享的临时目录、身份与种子数据（仅 `cfg(test)` 编译）。
//!
//! 每个测试一个独立目录与独立 SQLite 文件，`Drop` 时连同 WAL 与迁移前备份一起删除。
//! 种子绑定只经被测代码 [`crate::bindings::insert_binding`] 写入，不绕过它直接拼 SQL。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use vaultsyncd_domain::ids::{
    BindingId, DeviceId, EntryId, ID_LEN, OpId, RootId, ServerId, TxId, VersionId,
};
use vaultsyncd_domain::version::ServerEpoch;

use crate::bindings::{self, NewBinding, SyncMode};
use crate::db::Database;

/// 测试独占的临时目录；`Drop` 时整个删除。
pub(crate) struct TestDir {
    pub(crate) path: PathBuf,
}

impl TestDir {
    pub(crate) fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let index = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s030-{}-{label}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    pub(crate) fn database(&self) -> PathBuf {
        self.path.join("client.sqlite")
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
fn identity<T>(marker: u8, wrap: fn([u8; ID_LEN]) -> T) -> T {
    wrap([marker; ID_LEN])
}

pub(crate) fn binding_id(marker: u8) -> BindingId {
    identity(marker, BindingId::from_bytes)
}

pub(crate) fn server_id(marker: u8) -> ServerId {
    identity(marker, ServerId::from_bytes)
}

pub(crate) fn device_id(marker: u8) -> DeviceId {
    identity(marker, DeviceId::from_bytes)
}

pub(crate) fn root_id(marker: u8) -> RootId {
    identity(marker, RootId::from_bytes)
}

pub(crate) fn entry_id(marker: u8) -> EntryId {
    identity(marker, EntryId::from_bytes)
}

pub(crate) fn version_id(marker: u8) -> VersionId {
    identity(marker, VersionId::from_bytes)
}

pub(crate) fn op_id(marker: u8) -> OpId {
    identity(marker, OpId::from_bytes)
}

pub(crate) fn tx_id(marker: u8) -> TxId {
    identity(marker, TxId::from_bytes)
}

/// 在已开的事务里入队一条最小操作（无依赖、无基线），供需要 `pending_ops` 外键的测试使用。
pub(crate) async fn enqueue_op(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    binding: BindingId,
    op: OpId,
) {
    crate::queue::enqueue(
        tx,
        &crate::queue::NewPendingOp {
            op,
            binding,
            kind: "upload_content".to_owned(),
            entry: None,
            baseline_revision: None,
            baseline_version: None,
            depends_on: None,
            pinned_version: None,
        },
    )
    .await
    .expect("入队测试操作");
}

/// 建一个测试绑定并提交，返回它的身份。`local_path` 带上标记，避免两个绑定看起来一样。
pub(crate) async fn seed_binding(database: &Database, marker: u8) -> BindingId {
    let id = binding_id(marker);
    let mut tx = database.begin_write().await.expect("开始写事务");
    bindings::insert_binding(
        &mut tx,
        &NewBinding {
            id,
            server: server_id(0x01),
            device: device_id(0x02),
            root: root_id(0x03),
            local_path: format!(r"C:\vaultsyncd-test-{marker:02x}"),
            cache_path: format!(r"C:\vaultsyncd-test-{marker:02x}\.cache"),
            mode: SyncMode::OnDemand,
            rules_version: "rules-v1".to_owned(),
            server_epoch: ServerEpoch::new(4).expect("epoch"),
        },
    )
    .await
    .expect("写测试绑定");
    tx.commit().await.expect("提交测试绑定");
    id
}
