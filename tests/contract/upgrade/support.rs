//! 编译进两端数据库unit二进制的共享固定profile/原生文件装置，不发布到产品。
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
pub struct Run {
    pub path: PathBuf,
}
impl Run {
    pub fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::var_os("VAULTSYNCD_S091_RUN_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "s091-{label}-{}-{stamp}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).unwrap();
    }
}
pub fn previous(side: &str) -> Migrator {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/s091/profiles.json")).unwrap();
    let version = fixture[format!("{side}_schema")].as_i64().unwrap();
    let migrations: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| m.version <= version)
        .cloned()
        .collect();
    let expected = fixture["migrations"][side].as_array().unwrap();
    assert_eq!(migrations.len(), expected.len());
    for (m, pin) in migrations.iter().zip(expected) {
        assert_eq!(m.version, pin["version"].as_i64().unwrap());
        let hex: String = m.checksum.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            pin["sqlx_sha384"].as_str().unwrap(),
            "historical migration changed: {}",
            pin["file"]
        );
    }
    Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}
pub async fn fault_profile(
    base: &Migrator,
    root: &Path,
    label: &str,
    extra: &[(i64, &str)],
) -> Migrator {
    let dir = root.join(label);
    std::fs::create_dir(&dir).unwrap();
    for m in base.iter() {
        std::fs::write(
            dir.join(format!("{:04}_{}.sql", m.version, m.description)),
            m.sql.as_bytes(),
        )
        .unwrap();
    }
    for (n, sql) in extra {
        std::fs::write(dir.join(format!("{n:04}_s091_fault.sql")), sql).unwrap();
    }
    Migrator::new(dir.as_path()).await.unwrap()
}
pub async fn scalar<T>(path: &Path, sql: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, Sqlite> + sqlx::Type<Sqlite> + Send + Unpin + std::fmt::Debug,
{
    let mut db = connect_for_inspection(path)
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let value = sqlx::query_scalar(sql).fetch_one(&mut db).await.unwrap();
    db.close().await.unwrap();
    value
}
pub async fn snapshot(path: &Path, queries: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for q in queries {
        out.push(scalar::<String>(path, q).await);
    }
    out
}
pub fn copy_file(source: &Path, target: &Path) {
    assert!(!target.exists());
    std::fs::copy(source, target).unwrap();
    // FlushFileBuffers on Windows requires a writable handle.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .unwrap()
        .sync_all()
        .unwrap();
}
