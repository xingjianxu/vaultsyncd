//! S195 native NTFS/CFAPI admission regressions. No remote server is needed:
//! durable bindings and the real management detach command exercise directory ownership.
#![cfg(windows)]

use super::*;
use crate::config::BindingConfig;
use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt, path::PathBuf};
use vaultsyncd_agent_core::bindings::control::ExecutionGate;
use vaultsyncd_domain::{
    ids::OpId,
    state::{Dirty, Residency},
    version::Generation,
};
use vaultsyncd_local_ipc::generated::CommandParameters;
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::{Adapter, MetadataOnly},
        maintenance,
    },
    files::VaultHandle,
};
use vaultsyncd_transfer::hash::digest_of;

const OLD: BindingId = BindingId::from_bytes([31; 16]);
const NEW: BindingId = BindingId::from_bytes([32; 16]);
const DIRTY: EntryId = EntryId::from_bytes([33; 16]);
const PARTIAL: EntryId = EntryId::from_bytes([34; 16]);

struct Fixture {
    path: PathBuf,
    runtime: Runtime,
}

impl Fixture {
    async fn new() -> Self {
        // This module only runs on Windows: temp_dir must be a guest-local NTFS directory,
        // not a host shared folder. Host builds/caches are managed by the Windows devShell.
        let path = std::env::temp_dir().join(format!(
            "s195-rebind-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        for relative in [
            "state",
            "data/vault/child",
            "old-cache",
            "new-cache",
            "other",
        ] {
            std::fs::create_dir_all(path.join(relative)).unwrap();
        }
        let db = Arc::new(
            Database::open(path.join("state/client.sqlite"))
                .await
                .unwrap(),
        );
        let config: AgentConfig = serde_json::from_value(serde_json::json!({
            "server": {
                "id": ServerId::from_bytes([35; 16]).to_string(),
                "url": "https://example.invalid"
            },
            "device_id": DeviceId::from_bytes([36; 16]).to_string(),
            "state_dir": path.join("state"),
            "poll_millis": 250,
            "bindings": []
        }))
        .unwrap();
        let (report_wake, _) = mpsc::channel(1);
        let (connection_updates, _) = watch::channel(Vec::new());
        let (completed, _) = mpsc::unbounded_channel();
        let mut fixture = Self {
            path,
            runtime: Runtime {
                db,
                config,
                assignments: HashMap::new(),
                servers: BTreeMap::new(),
                blocked_servers: HashSet::new(),
                pending: HashMap::new(),
                report_wake,
                connection_updates,
                completed,
            },
        };
        let old = BindingConfig {
            id: OLD.to_string(),
            vault_id: VaultId::from_bytes([37; 16]).to_string(),
            path: fixture.path.join("data/vault"),
            cache: fixture.path.join("old-cache"),
            mode: "full".into(),
        };
        fixture.persist(&old).await;
        fixture.runtime.config.bindings.push(old);
        management::admit(&fixture.runtime.db, &fixture.runtime.config)
            .await
            .unwrap();
        fixture
    }

    async fn persist(&self, binding: &BindingConfig) {
        let config = &self.runtime.config;
        let mut tx = self.runtime.db.begin_write().await.unwrap();
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: BindingId::from_hex(&binding.id).unwrap(),
                server: ServerId::from_hex(&config.server.id).unwrap(),
                device: DeviceId::from_hex(&config.device_id).unwrap(),
                vault: VaultId::from_hex(&binding.vault_id).unwrap(),
                local_path: binding.path.to_string_lossy().into_owned(),
                cache_path: binding.cache.to_string_lossy().into_owned(),
                mode: SyncMode::Full,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    fn create(&self, path: &Path) -> Command {
        Command {
            name: CommandName::CreateBinding,
            parameters: Some(CommandParameters {
                binding_id: Some(NEW.to_string()),
                server_id: Some(self.runtime.config.server.id.clone()),
                vault_id: Some(VaultId::from_bytes([38; 16]).to_string()),
                local_path: Some(path.to_string_lossy().into_owned()),
                cache_path: Some(self.path.join("new-cache").to_string_lossy().into_owned()),
                mode: Some("full".into()),
                ..Default::default()
            }),
        }
    }

    async fn propose(&self, command: &Command) -> Result<BindingConfig, PortError> {
        management::new_binding(&self.runtime.db, &self.runtime.config, command).await
    }

    async fn detach(&self) -> ManagementResult {
        management::execute(
            &self.runtime.db,
            &self.runtime.config,
            None,
            HashMap::new(),
            &Command {
                name: CommandName::DetachBinding,
                parameters: Some(CommandParameters {
                    binding_id: Some(OLD.to_string()),
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap()
    }

    async fn restart(&mut self) {
        // Reopen SQLite as well as rebuilding config, rather than testing a stale in-memory view.
        self.runtime.db.pool().close().await;
        self.runtime.db = Arc::new(
            Database::open(self.runtime.config.state_dir.join("client.sqlite"))
                .await
                .unwrap(),
        );
        self.runtime.config.bindings.clear();
        self.runtime.assignments =
            management::load_bindings(&self.runtime.db, &mut self.runtime.config)
                .await
                .unwrap();
    }

    async fn close(self) {
        self.runtime.db.pool().close().await;
        let path = self.path;
        drop(self.runtime);
        for attempt in 0..100 {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => return,
                Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(cause) => panic!("cannot clean the closed S195 test fixture: {cause}"),
            }
        }
    }
}

#[tokio::test]
async fn local_pause_and_resume_do_not_wait_for_a_busy_server_worker() {
    let mut fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.runtime.config.server.url = format!("http://{}", listener.local_addr().unwrap());
    fixture.runtime.config.server.development_http = true;
    let server = ServerId::from_hex(&fixture.runtime.config.server.id).unwrap();
    fixture.runtime.start_server(
        server,
        server::Connection::open(fixture.runtime.config.server.clone()).unwrap(),
        false,
    );
    let request = |name, parameters| vaultsyncd_local_ipc::generated::Request {
        request_id: format!("s231-{name:?}"),
        ipc_version: vaultsyncd_local_ipc::version::supported(),
        command: Command { name, parameters },
    };
    // A real login reaches this local HTTP peer, which deliberately withholds its
    // response. The worker is now occupied by network I/O rather than a test stub.
    let (reply, login_response) = oneshot::channel();
    fixture
        .runtime
        .dispatch(crate::ipc::CommandRequest {
            request: request(
                CommandName::Login,
                Some(CommandParameters {
                    username: Some("s231-isolated-user".into()),
                    password: Some("s231-isolated-password".into()),
                    ..Default::default()
                }),
            ),
            reply,
        })
        .await;
    let (connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("server worker must reach the deliberately stalled peer")
        .unwrap();
    for (name, expected_paused) in [
        (CommandName::PauseBinding, true),
        (CommandName::ResumeBinding, false),
    ] {
        let (reply, response) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(2),
            fixture.runtime.dispatch(crate::ipc::CommandRequest {
                request: request(
                    name,
                    Some(CommandParameters {
                        binding_id: Some(OLD.to_string()),
                        ..Default::default()
                    }),
                ),
                reply,
            }),
        )
        .await
        .expect("local binding controls must not wait for the server worker");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), response)
                .await
                .expect("local control reply must arrive while the peer is stalled")
                .unwrap()
                .kind,
            vaultsyncd_local_ipc::generated::ResponseKind::Ok
        );
        assert_eq!(
            management::binding_list(&fixture.runtime.db).await.unwrap()[0].paused,
            expected_paused
        );
    }
    fixture.detach().await;
    let (reply, response) = oneshot::channel();
    fixture
        .runtime
        .dispatch(crate::ipc::CommandRequest {
            request: request(
                CommandName::ResumeBinding,
                Some(CommandParameters {
                    binding_id: Some(OLD.to_string()),
                    ..Default::default()
                }),
            ),
            reply,
        })
        .await;
    let result = tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .expect("detached rejection must also remain local")
        .unwrap();
    assert!(result.error.unwrap().message.contains("BINDING_UNBOUND"));
    let state = &management::binding_list(&fixture.runtime.db).await.unwrap()[0];
    assert!(state.detached && state.paused);
    // Release only this test's stalled peer and wait for its worker before closing SQLite.
    drop(connection);
    drop(listener);
    drop(login_response);
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.runtime.servers.remove(&server).unwrap().stop(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn maintenance_retains_detached_exports_without_trying_to_pause_old_binding() {
    let fixture = Fixture::new().await;
    fixture.detach().await;
    let destination = fixture.path.join("maintenance-recovery");
    let retained = b"S195-detached-cache-recovery";
    std::fs::write(fixture.path.join("old-cache/retained.bin"), retained).unwrap();
    std::fs::create_dir(&destination).unwrap();
    let result = crate::maintenance_prepare::prepare(
        &fixture.runtime.db,
        &fixture.runtime.config,
        HashMap::new(),
        destination.to_str().unwrap(),
    )
    .await
    .unwrap();
    // The export includes the durable state snapshot and the retained cache candidate.
    assert_eq!(result.exported_files.as_deref(), Some("2"));
    let package = destination.join(OLD.to_string());
    assert!(package.is_dir());
    assert!(std::fs::read_dir(&package).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        path.is_file() && std::fs::read(path).unwrap() == retained
    }));
    // The offline half must not infer 'not exported' from an empty list of active bindings.
    crate::maintenance::export_offline_recovery(
        &fixture.runtime.db,
        &fixture.runtime.config,
        &destination,
        true,
    )
    .await
    .unwrap();
    assert!(
        crate::maintenance::export_offline_recovery(
            &fixture.runtime.db,
            &fixture.runtime.config,
            &destination,
            false
        )
        .await
        .is_err()
    );
    assert!(management::binding_list(&fixture.runtime.db).await.unwrap()[0].detached);
    fixture.close().await;
}

#[tokio::test]
async fn fenced_server_cannot_release_a_path_through_fallback_detach() {
    let mut fixture = Fixture::new().await;
    let server = ServerId::from_hex(&fixture.runtime.config.server.id).unwrap();
    fixture.runtime.fence_server(server);
    let (reply, response) = oneshot::channel();
    fixture
        .runtime
        .dispatch(crate::ipc::CommandRequest {
            request: vaultsyncd_local_ipc::generated::Request {
                request_id: "s195-fenced-detach".into(),
                ipc_version: vaultsyncd_local_ipc::version::supported(),
                command: Command {
                    name: CommandName::DetachBinding,
                    parameters: Some(CommandParameters {
                        binding_id: Some(OLD.to_string()),
                        ..Default::default()
                    }),
                },
            },
            reply,
        })
        .await;
    assert!(
        response
            .await
            .unwrap()
            .error
            .unwrap()
            .message
            .contains("NEEDS_RECOVERY")
    );
    assert!(!management::binding_list(&fixture.runtime.db).await.unwrap()[0].detached);
    assert_eq!(
        fixture
            .propose(&fixture.create(&fixture.path.join("data/vault")))
            .await
            .unwrap_err()
            .code(),
        Some("PATH_OVERLAP")
    );
    fixture.close().await;
}

#[tokio::test]
async fn detached_restore_and_retry_are_blocked_before_persistence_or_http() {
    let fixture = Fixture::new().await;
    fixture.detach().await;
    let client =
        management::server_client(&fixture.runtime.config.server, AuthSession::new()).unwrap();
    let command = Command {
        name: CommandName::RestoreEntry,
        parameters: Some(CommandParameters {
            binding_id: Some(OLD.to_string()),
            entry_id: Some(DIRTY.to_string()),
            operation_id: Some(OpId::from_bytes([45; 16]).to_string()),
            base_revision: Some("1".into()),
            ..Default::default()
        }),
    };
    assert_eq!(
        crate::management_history::execute(
            &fixture.runtime.db,
            &fixture.runtime.config,
            &client,
            &command
        )
        .await
        .unwrap_err()
        .code(),
        Some("BINDING_BLOCKED")
    );
    assert_eq!(
        crate::management_history::retry_restore(
            &fixture.runtime.db,
            OLD,
            OpId::from_bytes([45; 16]),
            &client
        )
        .await
        .unwrap_err()
        .code(),
        Some("BINDING_BLOCKED")
    );
    let retry = Command {
        name: CommandName::RetryProblem,
        parameters: command.parameters.clone(),
    };
    assert_eq!(
        management::execute(
            &fixture.runtime.db,
            &fixture.runtime.config,
            None,
            HashMap::new(),
            &retry
        )
        .await
        .unwrap_err()
        .code(),
        Some("BINDING_BLOCKED")
    );
    let actions: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_actions")
        .fetch_one(fixture.runtime.db.pool())
        .await
        .unwrap();
    assert_eq!(actions, 0);
    fixture.close().await;
}

#[tokio::test]
async fn detached_root_still_rejects_nested_roots_and_foreign_registration() {
    let fixture = Fixture::new().await;
    let root = fixture.path.join("data/vault");
    fixture.detach().await;
    for path in [root.join("child"), fixture.path.join("data")] {
        assert_eq!(
            fixture
                .propose(&fixture.create(&path))
                .await
                .unwrap_err()
                .code(),
            Some("PATH_OVERLAP")
        );
    }
    let foreign = Catalog::new(
        BindingId::from_bytes([46; 16]),
        VaultId::from_bytes([47; 16]),
    );
    let mut adapter = Adapter::connect(&root, foreign, Arc::new(MetadataOnly)).unwrap();
    adapter.disconnect().unwrap();
    drop(adapter);
    let identity = maintenance::registered_identity(&root).unwrap();
    assert_eq!(
        fixture
            .propose(&fixture.create(&root))
            .await
            .unwrap_err()
            .code(),
        Some("BINDING_BLOCKED")
    );
    assert_eq!(maintenance::registered_identity(&root).unwrap(), identity);
    maintenance::unregister_materialized(&root).unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn detached_root_accepts_new_identity_and_restart_preserves_old_recovery_state() {
    let mut fixture = Fixture::new().await;
    let root = fixture.path.join("data/vault");
    let cached = fixture.path.join("old-cache/complete-content");
    std::fs::write(root.join("draft.txt"), b"unconfirmed local bytes").unwrap();
    std::fs::write(&cached, b"complete cached bytes must survive").unwrap();
    let dirty = entries::LocalEntry {
        entry: DIRTY,
        generation: Generation::new(7).unwrap(),
        dirty: Dirty::AwaitingCommit,
        residency: Residency::Materialized,
        fingerprint: Some(vec![1, 2, 3]),
        file_identity: Some(vec![4, 5, 6]),
    };
    let partial = entries::LocalEntry {
        entry: PARTIAL,
        generation: Generation::new(8).unwrap(),
        dirty: Dirty::Clean,
        residency: Residency::Partial,
        fingerprint: None,
        file_identity: None,
    };
    let mut tx = fixture.runtime.db.begin_write().await.unwrap();
    entries::put_local(&mut tx, OLD, &dirty).await.unwrap();
    entries::put_local(&mut tx, OLD, &partial).await.unwrap();
    tx.commit().await.unwrap();

    // Exact/child/parent overlap stays forbidden while the original binding is managed.
    for path in [&root, &root.join("child"), &fixture.path.join("data")] {
        assert_eq!(
            fixture
                .propose(&fixture.create(path))
                .await
                .unwrap_err()
                .code(),
            Some("PATH_OVERLAP"),
            "managed root admitted overlapping path {}",
            path.display()
        );
    }
    let report = fixture.detach().await;
    assert_eq!(report.unconfirmed, Some(vec![DIRTY.to_string()]));
    assert_eq!(report.incomplete, Some(vec![PARTIAL.to_string()]));
    assert_eq!(
        BindingControls::new(fixture.runtime.db.clone())
            .execution_gate(OLD, None)
            .await
            .unwrap(),
        ExecutionGate::Unbound
    );

    let replacement = fixture.propose(&fixture.create(&root)).await.unwrap();
    assert_eq!(replacement.id, NEW.to_string());
    assert_ne!(replacement.id, fixture.runtime.config.bindings[0].id);
    fixture.persist(&replacement).await;
    fixture.runtime.config.bindings.push(replacement);
    management::admit(&fixture.runtime.db, &fixture.runtime.config)
        .await
        .unwrap();
    fixture.restart().await;
    assert_eq!(fixture.runtime.config.bindings.len(), 2);
    assert_eq!(fixture.runtime.assignments.len(), 2);
    assert!(
        fixture
            .runtime
            .config
            .bindings
            .iter()
            .any(|b| b.id == OLD.to_string())
    );
    assert!(
        fixture
            .runtime
            .config
            .bindings
            .iter()
            .any(|b| b.id == NEW.to_string())
    );
    management::admit(&fixture.runtime.db, &fixture.runtime.config)
        .await
        .unwrap();
    let records = management::binding_list(&fixture.runtime.db).await.unwrap();
    assert_eq!(records.len(), 2);
    let old = records
        .iter()
        .find(|b| b.binding_id == OLD.to_string())
        .unwrap();
    assert!(old.detached && old.paused);
    assert!(
        !records
            .iter()
            .find(|b| b.binding_id == NEW.to_string())
            .unwrap()
            .detached
    );
    let mut conn = fixture.runtime.db.pool().acquire().await.unwrap();
    assert_eq!(
        entries::local_entry(&mut conn, OLD, DIRTY).await.unwrap(),
        Some(dirty)
    );
    assert_eq!(
        entries::local_entry(&mut conn, OLD, PARTIAL).await.unwrap(),
        Some(partial)
    );
    assert!(
        entries::local_entry(&mut conn, NEW, DIRTY)
            .await
            .unwrap()
            .is_none()
    );
    drop(conn);
    // I02/I05/I11/I13: no local bytes, incomplete evidence, or complete cache were reassigned/deleted.
    assert_eq!(
        std::fs::read(root.join("draft.txt")).unwrap(),
        b"unconfirmed local bytes"
    );
    assert_eq!(
        std::fs::read(cached).unwrap(),
        b"complete cached bytes must survive"
    );
    fixture.close().await;
}

#[tokio::test]
async fn detached_binding_keeps_state_and_historical_cache_outside_new_roots_and_caches() {
    let fixture = Fixture::new().await;
    fixture.detach().await;
    let old_cache = fixture.path.join("old-cache");
    std::fs::create_dir(old_cache.join("child")).unwrap();
    std::fs::create_dir(fixture.runtime.config.state_dir.join("child")).unwrap();
    for protected in [&old_cache, &fixture.runtime.config.state_dir] {
        for path in [protected.clone(), protected.join("child")] {
            assert_eq!(
                fixture
                    .propose(&fixture.create(&path))
                    .await
                    .unwrap_err()
                    .code(),
                Some("PATH_OVERLAP"),
                "detached recovery storage admitted as root: {}",
                path.display()
            );
            let mut command = fixture.create(&fixture.path.join("other"));
            command.parameters.as_mut().unwrap().cache_path =
                Some(path.to_string_lossy().into_owned());
            assert_eq!(
                fixture.propose(&command).await.unwrap_err().code(),
                Some("PATH_OVERLAP")
            );
        }
    }
    // A new root enclosing the old cache/state is not made safe by detaching its binding.
    assert_eq!(
        fixture
            .propose(&fixture.create(&fixture.path))
            .await
            .unwrap_err()
            .code(),
        Some("PATH_OVERLAP")
    );
    // Neither retaining the directory nor detaching it makes an old identity reusable.
    let mut reused_id = fixture.create(&fixture.path.join("data/vault"));
    reused_id.parameters.as_mut().unwrap().binding_id = Some(OLD.to_string());
    assert_eq!(
        fixture.propose(&reused_id).await.unwrap_err().code(),
        Some("ALREADY_EXISTS")
    );
    fixture.close().await;
}

#[tokio::test]
async fn deleted_detached_root_does_not_block_restart_but_its_cache_stays_reserved() {
    let mut fixture = Fixture::new().await;
    let root = fixture.path.join("data/vault");
    fixture.detach().await;
    std::fs::remove_dir_all(&root).unwrap();
    fixture.restart().await;
    assert_eq!(fixture.runtime.config.bindings.len(), 1);
    assert_eq!(fixture.runtime.config.bindings[0].id, OLD.to_string());
    management::admit(&fixture.runtime.db, &fixture.runtime.config)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .propose(&fixture.create(&fixture.path.join("old-cache")))
            .await
            .unwrap_err()
            .code(),
        Some("PATH_OVERLAP")
    );
    // Recreating the ordinary directory allows a genuinely new identity, not the old one.
    std::fs::create_dir(&root).unwrap();
    assert_eq!(
        fixture.propose(&fixture.create(&root)).await.unwrap().id,
        NEW.to_string()
    );
    fixture.close().await;
}

#[tokio::test]
async fn unavailable_active_root_or_any_historical_cache_fails_closed() {
    for (relative, detached) in [
        ("data/vault", false),
        ("old-cache", false),
        ("old-cache", true),
    ] {
        let fixture = Fixture::new().await;
        if detached {
            fixture.detach().await;
        }
        let unavailable = fixture.path.join(relative);
        // A real Windows sharing denial, not a fake filesystem result. LIST_DIRECTORY makes
        // the exclusive handle conflict with the metadata-only VaultHandle traversal.
        let exclusive = OpenOptions::new()
            .access_mode(1)
            .share_mode(0)
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS: open the directory itself.
            .open(&unavailable)
            .unwrap();
        assert!(
            VaultHandle::open(&unavailable).is_err(),
            "fault must deny native directory access"
        );
        assert_eq!(
            management::admit(&fixture.runtime.db, &fixture.runtime.config)
                .await
                .unwrap_err()
                .code(),
            Some("LOCAL_PATH_UNAVAILABLE")
        );
        assert_eq!(
            fixture
                .propose(&fixture.create(&fixture.path.join("other")))
                .await
                .unwrap_err()
                .code(),
            Some("LOCAL_PATH_UNAVAILABLE")
        );
        drop(exclusive);
        management::admit(&fixture.runtime.db, &fixture.runtime.config)
            .await
            .unwrap();
        std::fs::remove_dir_all(&unavailable).unwrap();
        assert_eq!(
            management::admit(&fixture.runtime.db, &fixture.runtime.config)
                .await
                .unwrap_err()
                .code(),
            Some("LOCAL_PATH_UNAVAILABLE")
        );
        assert!(
            fixture
                .propose(&fixture.create(&fixture.path.join("other")))
                .await
                .is_err()
        );
        fixture.close().await;
    }
}

#[tokio::test]
async fn uncommitted_config_reservation_is_not_mistaken_for_a_detached_binding() {
    let mut fixture = Fixture::new().await;
    fixture.detach().await;
    let root = fixture.path.join("other");
    // Runtime reserves config paths before a create worker commits its binding row.
    fixture.runtime.config.bindings.push(BindingConfig {
        id: BindingId::from_bytes([39; 16]).to_string(),
        vault_id: VaultId::from_bytes([40; 16]).to_string(),
        path: root.clone(),
        cache: fixture.path.join("pending-cache"),
        mode: "full".into(),
    });
    std::fs::create_dir(fixture.path.join("pending-cache")).unwrap();
    std::fs::create_dir(root.join("child")).unwrap();
    for path in [&root, &root.join("child")] {
        assert_eq!(
            fixture
                .propose(&fixture.create(path))
                .await
                .unwrap_err()
                .code(),
            Some("PATH_OVERLAP")
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn full_old_cfapi_placeholder_becomes_ordinary_before_new_binding_adopts_it() {
    let mut fixture = Fixture::new().await;
    let root = fixture.path.join("data/vault");
    let bytes: Vec<u8> = (0..65536).map(|n| (n % 251) as u8).collect();
    std::fs::write(root.join("complete.bin"), &bytes).unwrap();
    let old = Identity {
        binding: OLD,
        vault: VaultId::from_hex(&fixture.runtime.config.bindings[0].vault_id).unwrap(),
        entry: DIRTY,
        version: VersionId::from_bytes([41; 16]),
    };
    let mut catalog = Catalog::new(old.binding, old.vault);
    catalog.insert(old).unwrap();
    let mut adapter = Adapter::connect(&root, catalog, Arc::new(MetadataOnly)).unwrap();
    let ordinary = adapter.inspect("complete.bin").unwrap();
    let complete = adapter
        .adopt_complete(
            "complete.bin",
            &ordinary,
            old,
            digest_of(&bytes),
            bytes.len() as u64,
        )
        .unwrap();
    assert!(complete.placeholder());
    assert!(complete.cloud.as_ref().unwrap().on_disk >= bytes.len() as u64);
    assert_eq!(
        Identity::decode(&complete.cloud.unwrap().identity).unwrap(),
        old
    );
    adapter.disconnect().unwrap();
    drop(adapter); // No live old connection/metadata handles survive into the handover.
    fixture.detach().await;
    assert_eq!(
        maintenance::registered_identity(&root).unwrap(),
        Some(Catalog::new(old.binding, old.vault).vault_blob())
    );

    let providers =
        crate::management_paths::provider_bindings(&fixture.runtime.db, &fixture.runtime.config)
            .await
            .unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].id, OLD.to_string());
    maintenance::check_materialized(&providers[0].path).unwrap();
    let replacement = fixture.propose(&fixture.create(&root)).await.unwrap();
    let ordinary = VaultHandle::open(&root)
        .unwrap()
        .inspect("complete.bin")
        .unwrap();
    assert!(
        !ordinary.placeholder(),
        "old provider identity must not cross binding scopes"
    );
    assert_eq!(ordinary.identity, complete.identity);
    assert_eq!(std::fs::read(root.join("complete.bin")).unwrap(), bytes);
    assert!(maintenance::registered_identity(&root).unwrap().is_none());
    fixture.persist(&replacement).await;
    fixture.runtime.config.bindings.push(replacement.clone());
    let new = Identity {
        binding: NEW,
        vault: VaultId::from_hex(&replacement.vault_id).unwrap(),
        entry: EntryId::from_bytes([42; 16]),
        version: VersionId::from_bytes([43; 16]),
    };
    let mut catalog = Catalog::new(new.binding, new.vault);
    catalog.insert(new).unwrap();
    let mut adapter = Adapter::connect(&root, catalog, Arc::new(MetadataOnly)).unwrap();
    let adopted = adapter
        .adopt_complete(
            "complete.bin",
            &ordinary,
            new,
            digest_of(&bytes),
            bytes.len() as u64,
        )
        .unwrap();
    assert_eq!(
        Identity::decode(&adopted.cloud.unwrap().identity).unwrap(),
        new
    );
    assert_eq!(std::fs::read(root.join("complete.bin")).unwrap(), bytes);
    adapter.disconnect().unwrap();
    drop(adapter);
    let providers =
        crate::management_paths::provider_bindings(&fixture.runtime.db, &fixture.runtime.config)
            .await
            .unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].id, NEW.to_string());
    maintenance::revert_materialized(&root).unwrap();
    maintenance::unregister_materialized(&root).unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn partial_old_cfapi_placeholder_blocks_rebind_without_unregistering_or_losing_identity() {
    let fixture = Fixture::new().await;
    let root = fixture.path.join("data/vault");
    let old = Identity {
        binding: OLD,
        vault: VaultId::from_hex(&fixture.runtime.config.bindings[0].vault_id).unwrap(),
        entry: PARTIAL,
        version: VersionId::from_bytes([44; 16]),
    };
    let mut catalog = Catalog::new(old.binding, old.vault);
    catalog.insert(old).unwrap();
    let mut adapter = Adapter::connect(&root, catalog, Arc::new(MetadataOnly)).unwrap();
    let before = adapter.create("cold.bin", old, 65536, false).unwrap();
    assert!(before.cloud.as_ref().unwrap().on_disk < before.size);
    adapter.disconnect().unwrap();
    drop(adapter);
    fixture.detach().await;
    let providers =
        crate::management_paths::provider_bindings(&fixture.runtime.db, &fixture.runtime.config)
            .await
            .unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].id, OLD.to_string());
    assert!(maintenance::check_materialized(&providers[0].path).is_err());
    assert_eq!(
        fixture
            .propose(&fixture.create(&root))
            .await
            .unwrap_err()
            .code(),
        Some("MODE_REQUIRES_LOCAL_COPY")
    );
    let after = VaultHandle::open(&root)
        .unwrap()
        .inspect("cold.bin")
        .unwrap();
    assert_eq!(after.identity, before.identity);
    assert_eq!(after.size, before.size);
    assert_eq!(after.cloud, before.cloud);
    assert_eq!(
        maintenance::registered_identity(&root).unwrap(),
        Some(Catalog::new(old.binding, old.vault).vault_blob())
    );
    assert_eq!(
        management::binding_list(&fixture.runtime.db)
            .await
            .unwrap()
            .len(),
        1
    );
    // Explicitly remove only this test-created empty placeholder to clean up the registration.
    // The failed management operation above must leave it untouched.
    std::fs::remove_file(root.join("cold.bin")).unwrap();
    maintenance::unregister_materialized(&root).unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn unreadable_detach_controls_never_release_an_active_directory() {
    let fixture = Fixture::new().await;
    sqlx::query("ALTER TABLE binding_controls RENAME TO s195_unavailable_controls")
        .execute(fixture.runtime.db.pool())
        .await
        .unwrap();
    assert!(
        management::admit(&fixture.runtime.db, &fixture.runtime.config)
            .await
            .is_err()
    );
    assert!(
        fixture
            .propose(&fixture.create(&fixture.path.join("data/vault")))
            .await
            .is_err()
    );
    sqlx::query("ALTER TABLE s195_unavailable_controls RENAME TO binding_controls")
        .execute(fixture.runtime.db.pool())
        .await
        .unwrap();
    management::admit(&fixture.runtime.db, &fixture.runtime.config)
        .await
        .unwrap();
    fixture.close().await;
}
