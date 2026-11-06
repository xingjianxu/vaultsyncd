//! Native Linux child holds real Create publication boundaries for parent SIGKILL acceptance.
use std::{io::Write, path::PathBuf};
use vaultsyncd_domain::{
    ids::{DeviceId, EntryId, IntentId, OpId, RootId, UploadId, UserId, VersionId},
    version::{Generation, ServerEpoch},
};
use vaultsyncd_server::{
    create_file,
    executor::{ExecutorContext, SqlIntentLog, WorkTreeTarget},
};
use vaultsyncd_server_core::{
    api::operations::{ConflictPolicy, OperationKind, OperationRequest},
    commit::intent::{NewPutIntent, TargetAfter, TargetBefore, publish_put},
    objects::{ContentDigest, ExpectedContent},
    ports::intent_log::IntentLog,
};
use vaultsyncd_server_store::{
    db::Database,
    operations::{OperationKey, request_digest},
};
#[tokio::main]
async fn main() {
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(std::env::args_os().nth(1).unwrap()).unwrap())
            .unwrap();
    let text = |key: &str| fixture[key].as_str().unwrap();
    let state = PathBuf::from(text("state"));
    let db = Database::open(state.join("server.sqlite")).await.unwrap();
    let root = RootId::from_hex(text("root")).unwrap();
    let owner: Vec<u8> = sqlx::query_scalar("SELECT owner_id FROM roots WHERE id=?")
        .bind(root.as_bytes().as_slice())
        .fetch_one(db.pool())
        .await
        .unwrap();
    let owner = UserId::from_bytes(owner.try_into().unwrap());
    let request = OperationRequest {
        op: OpId::from_hex(text("op")).unwrap(),
        device: DeviceId::from_hex(text("device")).unwrap(),
        kind: OperationKind::Create,
        expected_epoch: ServerEpoch::from_decimal(text("epoch")).unwrap(),
        entry: None,
        base_revision: None,
        base_version: None,
        upload: Some(UploadId::from_hex(text("upload")).unwrap()),
        local_generation: Some(Generation::new(9).unwrap()),
        conflict_policy: ConflictPolicy::PreserveBoth,
        parent: Some(EntryId::from_hex(text("parent")).unwrap()),
        name: Some(text("name").into()),
        expected_absent: Some(true),
        subtree_token: None,
        version: None,
    };
    let key = OperationKey {
        root,
        device: request.device,
        op: request.op,
    };
    let fields = request.digest_fields();
    let pairs: Vec<_> = fields.iter().map(|(k, v)| (*k, v.as_bytes())).collect();
    let digest = request_digest(&pairs).unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    let source = vaultsyncd_server_store::uploads::upload_in_scope(
        &mut conn,
        owner,
        request.upload.unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    drop(conn);
    let content =
        ExpectedContent::new(ContentDigest::from_bytes(source.hash), source.expected_size);
    let executor = ExecutorContext::new(db.clone(), &state, None);
    let at = vaultsyncd_server::executor::server_now().unwrap();
    if text("phase") == "committed" {
        create_file::execute(&executor, owner, key, digest, &request, content, at)
            .await
            .unwrap();
    } else {
        executor.begin_operation(owner, key, digest).await.unwrap();
        let binding = create_file::recovery_binding(
            key,
            &request,
            EntryId::from_bytes([0x91; 16]),
            VersionId::from_bytes([0x92; 16]),
        )
        .unwrap();
        let mut log = SqlIntentLog::new(db.clone(), owner).bound(binding);
        let new = NewPutIntent {
            intent_id: IntentId::from_bytes([0x93; 16]),
            root,
            target_path: text("name").into(),
            staging_path: "create-crash-stage".into(),
            before: TargetBefore::Absent,
            after: TargetAfter {
                digest: content.digest,
                size: content.bytes,
            },
        };
        if text("phase") == "prepared" {
            log.begin_put(&new, at).await.unwrap();
        } else {
            let mut target = WorkTreeTarget::new(
                executor.root_handle(owner, root).await.unwrap(),
                executor.objects(),
            );
            publish_put(&mut log, &mut target, at, &new).await.unwrap();
        }
    }
    let mut marker = std::fs::File::create(text("marker")).unwrap();
    marker.write_all(b"boundary persisted").unwrap();
    marker.sync_all().unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
