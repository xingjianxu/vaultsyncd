//! Offline, disposable S097 fixture: persist an actual put boundary, never edit a live DB.
use rand_core::{OsRng, RngCore};
use std::{fs, io::Write, path::PathBuf, sync::Arc};
use vaultsyncd_domain::ids::{DeviceId, IntentId, OpId, VaultId};
use vaultsyncd_platform_linux_server::files::{ResolveMode, VaultHandle, hash};
use vaultsyncd_server::{
    executor::{SqlIntentLog, WorkTreeTarget},
    journal::Binding,
};
use vaultsyncd_server_core::{
    commit::intent::{NewPutIntent, PutPhase, TargetAfter, TargetBefore, TargetSnapshot},
    objects::{ContentDigest, ObjectLayout},
    ports::{
        intent_log::IntentLog,
        target::{PublishRequest, TargetObservation, TargetPort},
    },
};
use vaultsyncd_server_store::{
    db::Database,
    entries,
    operations::{self, NewOperation, OperationKey},
    state_lock::StateLock,
    vaults,
    versions::{self, NewObject},
};

fn id() -> [u8; 16] {
    let mut bytes = [0; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 5, "state root relative-path prepared|published");
    let state = PathBuf::from(&args[1]).canonicalize()?;
    assert!(
        state.starts_with("/data")
            && state.components().any(|c| c
                .as_os_str()
                .to_string_lossy()
                .starts_with("vs-s097-fences-")),
        "only disposable S097 fixture"
    );
    let published = match args[4].as_str() {
        "prepared" => false,
        "published" => true,
        _ => panic!("invalid boundary"),
    };
    let _lock = StateLock::try_acquire_exclusive(&state)?;
    let db = Database::open(state.join("server.sqlite")).await?;
    let vault_id: VaultId = args[2].parse()?;
    let mut conn = db.pool().acquire().await?;
    let vault = vaults::all_vaults(&mut conn)
        .await?
        .into_iter()
        .find(|r| r.id == vault_id)
        .expect("fixture Vault");
    let entry = entries::entry_by_path(&mut conn, vault.owner, vault.id, &args[3])
        .await?
        .expect("observed fixture file");
    drop(conn);
    let handle = Arc::new(VaultHandle::open(
        std::path::Path::new(&vault.absolute_path),
        ResolveMode::Auto,
    )?);
    let layout = ObjectLayout::new(state.join("objects"));
    let mut target = WorkTreeTarget::new(handle.clone(), layout.clone());
    let TargetObservation::Present {
        identity,
        digest: before_digest,
    } = target.observe(&args[3])?
    else {
        panic!("missing fixture before");
    };
    let content = format!(
        "S097 actual {} boundary {}\n",
        args[4],
        OpId::from_bytes(id())
    );
    let digest = ContentDigest::from_bytes(hash::digest(content.as_bytes()));
    let object = layout.object_path(&digest);
    fs::create_dir_all(object.parent().unwrap())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&object)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    fs::File::open(object.parent().unwrap())?.sync_all()?;
    let at = vaultsyncd_domain::version::UtcTimestamp::from_unix_seconds(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            .try_into()?,
    )?;
    let key = OperationKey {
        vault: vault.id,
        device: DeviceId::from_bytes(id()),
        op: OpId::from_bytes(id()),
    };
    let request_digest = operations::request_digest(&[("fixture", content.as_bytes())])?;
    let mut tx = db.begin_write().await?;
    versions::record_object(
        &mut tx,
        vault.owner,
        &NewObject {
            vault: vault.id,
            hash: *digest.as_bytes(),
            size: content.len() as u64,
            storage_path: object.strip_prefix(&state)?.to_string_lossy().into(),
            verified_at: at,
        },
    )
    .await?;
    assert!(matches!(
        operations::begin_operation(
            &mut tx,
            vault.owner,
            &NewOperation {
                key,
                digest: request_digest
            }
        )
        .await?,
        operations::BeginOperation::Started
    ));
    tx.commit().await?;
    let binding = Binding::new(
        key,
        "put",
        serde_json::json!({"entry":entry.id.to_hex(),"revision":entry.revision.to_string()}),
    );
    let mut log = SqlIntentLog::new(db.clone(), vault.owner).bound(binding);
    let new = NewPutIntent {
        intent_id: IntentId::from_bytes(id()),
        vault: vault.id,
        target_path: args[3].clone(),
        staging_path: format!("s097-{}", key.op),
        before: TargetBefore::Present(TargetSnapshot {
            entry: entry.id,
            revision: entry.revision.get(),
            digest: before_digest,
            identity,
        }),
        after: TargetAfter {
            digest,
            size: content.len() as u64,
        },
    };
    log.begin_put(&new, at).await?;
    if published {
        target.publish(&PublishRequest {
            target_path: new.target_path.clone(),
            staging_path: new.staging_path.clone(),
            digest,
            size: content.len() as u64,
        })?;
        log.advance_put(new.intent_id, PutPhase::Published, at)
            .await?;
    }
    println!(
        "{}",
        serde_json::json!({"vault":vault.id.to_hex(),"entry":entry.id.to_hex(),"op":key.op.to_hex(),"intent":new.intent_id.to_hex(),"phase":args[4],"before_device":identity.device,"before_inode":identity.inode,"before_hash":before_digest.to_hex(),"after_hash":digest.to_hex(),"mount_id":handle.snapshot().mount_id,"dev_minor":handle.snapshot().dev_minor})
    );
    drop(log);
    db.close().await;
    Ok(())
}
