use super::*;
use std::{path::PathBuf, sync::Arc};
use vaultsyncd_client_store::bindings::{self, NewBinding, SyncMode};
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{DeviceId, RootId, ServerId},
    version::ServerEpoch,
};
pub(crate) const B: BindingId = BindingId::from_bytes([1; 16]);
pub(crate) struct Dir(pub PathBuf);
impl Dir {
    pub(crate) fn new() -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "vaultsyncd-s064-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub(crate) async fn setup(dir: &Dir) -> Arc<Database> {
    let db = Arc::new(Database::open(dir.0.join("state.sqlite")).await.unwrap());
    let mut tx = db.begin_write().await.unwrap();
    bindings::insert_binding(
        &mut tx,
        &NewBinding {
            id: B,
            server: ServerId::from_bytes([2; 16]),
            device: DeviceId::from_bytes([3; 16]),
            root: RootId::from_bytes([4; 16]),
            local_path: "C:/local".into(),
            cache_path: "C:/cache".into(),
            mode: SyncMode::OnDemand,
            rules_version: "v1".into(),
            server_epoch: ServerEpoch::new(1).unwrap(),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    db
}
#[tokio::test]
async fn pending_events_and_open_release_cannot_be_green() {
    let d = Dir::new();
    let db = setup(&d).await;
    sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind) VALUES (?,?, 'apply_remote')")
        .bind([9u8; 16].as_slice())
        .bind(B.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    assert!(!snapshot(&db, B).await.unwrap().synchronized());
    sqlx::query("DELETE FROM pending_ops")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE bindings SET received_seq=3,applied_seq=1")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(snapshot(&db, B).await.unwrap().event_lag, 2);
    sqlx::query("UPDATE bindings SET applied_seq=3")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO controlled_releases(op_id,binding_id,target_path,intent_id,journal) VALUES (?,?, 'x',NULL,'{}')").bind([8u8;16].as_slice()).bind(B.as_bytes().as_slice()).execute(db.pool()).await.unwrap();
    assert_eq!(snapshot(&db, B).await.unwrap().recovery, 1);
}
#[tokio::test]
async fn root_dedup_offline_silent_recovery_and_recurrence() {
    let d = Dir::new();
    let db = setup(&d).await;
    let root = RootId::from_bytes([4; 16]);
    for _ in 0..100 {
        let r = issues::report(&db, root, ErrorCode::PlaceholderOffline, false)
            .await
            .unwrap();
        assert!(!r.notify);
    }
    assert!(
        issues::report(&db, root, ErrorCode::PlaceholderOffline, true)
            .await
            .unwrap()
            .explain_open
    );
    assert!(
        issues::report(&db, root, ErrorCode::InsufficientStorage, false)
            .await
            .unwrap()
            .notify
    );
    for _ in 0..10 {
        assert!(
            !issues::report(&db, root, ErrorCode::InsufficientStorage, false)
                .await
                .unwrap()
                .notify
        );
    }
    assert!(
        issues::resolve_root(&db, root, ErrorCode::InsufficientStorage)
            .await
            .unwrap()
    );
    assert!(
        issues::report(&db, root, ErrorCode::InsufficientStorage, false)
            .await
            .unwrap()
            .notify
    );
    let mut conn = db.pool().acquire().await.unwrap();
    assert_eq!(
        vaultsyncd_client_store::notifications::notifications(&mut conn)
            .await
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn diagnostic_limit_never_exports_free_text() {
    let records = vec![diagnostics::Diagnostic {
        op: None,
        binding: Some(B),
        code: ErrorCode::InsufficientStorage,
        duration_ms: 10,
    }];
    for limit in 0..300 {
        let bytes = diagnostics::bounded(&records, limit);
        assert!(bytes.len() <= limit);
        std::str::from_utf8(&bytes).unwrap();
    }
    assert!(!diagnostics::bounded(&records, 300).is_empty());
}
#[tokio::test]
async fn full_without_controls_or_with_incomplete_content_cannot_be_complete() {
    let d = Dir::new();
    let db = setup(&d).await;
    sqlx::query("UPDATE bindings SET mode='full'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(snapshot(&db, B).await.unwrap().mode_pending);
    assert!(!snapshot(&db, B).await.unwrap().synchronized());
}
#[tokio::test]
async fn unknown_materialized_content_and_initial_binding_are_visible() {
    let d = Dir::new();
    let db = setup(&d).await;
    assert!(!snapshot(&db, B).await.unwrap().synchronized());
    sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency) VALUES (?,?,0,'clean','materialized')").bind(B.as_bytes().as_slice()).bind([7u8;16].as_slice()).execute(db.pool()).await.unwrap();
    assert_eq!(snapshot(&db, B).await.unwrap().unknown, 2);
}
