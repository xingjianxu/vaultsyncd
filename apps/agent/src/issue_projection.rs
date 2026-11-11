//! Aggregate all bindings before resolving a root cause. Notification records are not their own cause.
use crate::{config::AgentConfig, local::error};
use std::collections::{HashMap, HashSet};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{BindingId, ServerId, VaultId},
};
use vaultsyncd_local_ipc::generated::Problem;
use vaultsyncd_transfer::ports::PortError;
type ConflictNoticeRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64);

pub(crate) async fn sync(
    db: &Database,
    config: &AgentConfig,
    startup: &HashMap<BindingId, Problem>,
    cycles: &HashMap<BindingId, Problem>,
) -> Result<Vec<Problem>, PortError> {
    let mut origins = crate::management::problems(db, None).await?;
    let conflicts: Vec<ConflictNoticeRow>=sqlx::query_as("SELECT c.binding_id,b.server_id,b.vault_id,c.seq FROM received_changes c JOIN bindings b ON b.id=c.binding_id WHERE EXISTS(SELECT 1 FROM json_each(c.events_json) e WHERE json_extract(e.value,'$.kind')='conflict')")
        .fetch_all(db.pool()).await.map_err(error)?;
    for (binding, server, vault, seq) in conflicts {
        let server = ServerId::from_bytes(
            server
                .try_into()
                .map_err(|_| error("invalid conflict server"))?,
        );
        let vault = VaultId::from_bytes(
            vault
                .try_into()
                .map_err(|_| error("invalid conflict vault"))?,
        );
        let marker =
            serde_json::json!({"kind":"conflict_notice","vault":vault.to_string(),"seq":seq})
                .to_string();
        let digest = vaultsyncd_transfer::hash::digest_of(
            format!(
                "S083-conflict:{}:{seq}",
                binding
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            )
            .as_bytes(),
        );
        let mut tx = db.begin_write().await.map_err(error)?;
        let old: Option<String> =
            sqlx::query_scalar("SELECT specification FROM agent_actions WHERE op_id=?")
                .bind(&digest[..16])
                .fetch_optional(&mut *tx)
                .await
                .map_err(error)?;
        if let Some(old) = old {
            if old != marker {
                return Err(error("conflict notice identity reused"));
            }
        } else {
            sqlx::query("INSERT INTO agent_actions(op_id,binding_id,specification) VALUES(?,?,?)")
                .bind(&digest[..16])
                .bind(binding)
                .bind(marker)
                .execute(&mut *tx)
                .await
                .map_err(error)?;
            vaultsyncd_client_store::notifications::raise(
                &mut tx,
                &crate::management_notices::problem(
                    server,
                    vault,
                    ErrorCode::NeedsRecovery,
                    "CONFLICT_COPY",
                ),
            )
            .await
            .map_err(error)?;
        }
        tx.commit().await.map_err(error)?;
    }
    origins.retain(|p| !p.problem_id.starts_with("notification:"));
    origins.extend(startup.values().chain(cycles.values()).cloned());
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT lower(hex(id)),lower(hex(server_id)),lower(hex(vault_id)) FROM bindings WHERE device_id=?",
    )
    .bind(vaultsyncd_domain::ids::DeviceId::from_hex(&config.device_id).map_err(error)?.as_bytes().as_slice())
    .fetch_all(db.pool()).await.map_err(error)?;
    let mut bindings = HashMap::new();
    let mut scopes = HashSet::new();
    for (binding, server, vault) in rows {
        let scope = (
            ServerId::from_hex(&server).map_err(error)?,
            VaultId::from_hex(&vault).map_err(error)?,
        );
        bindings.insert(binding, scope);
        scopes.insert(scope);
    }
    let mut active = HashSet::new();
    for problem in &origins {
        if let Some(code) =
            ErrorCode::from_code(&problem.code).filter(|code| code.class().is_severe())
            && let Some(&(server, vault)) = bindings.get(&problem.binding_id)
        {
            active.insert((server, vault, code));
        }
    }
    for &(server, vault, code) in &active {
        crate::management_notices::raise(db, server, vault, code, code.as_str()).await?;
    }
    let notices = crate::management_notices::list(db).await?;
    for notice in notices {
        if notice.state != "active" || notice.cause == "CONFLICT_COPY" {
            continue;
        }
        if let Some(server) = notice
            .server_id
            .as_deref()
            .and_then(|id| ServerId::from_hex(id).ok())
            && let Some(vault) = notice
                .vault_id
                .as_deref()
                .and_then(|id| VaultId::from_hex(id).ok())
            && let Some(code) = ErrorCode::from_code(&notice.cause)
            && scopes.contains(&(server, vault))
            && !active.contains(&(server, vault, code))
        {
            let mut tx = db.begin_write().await.map_err(error)?;
            vaultsyncd_client_store::notifications::resolve(&mut tx, &notice.key)
                .await
                .map_err(error)?;
            tx.commit().await.map_err(error)?;
        }
    }
    Ok(startup.values().chain(cycles.values()).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaultsyncd_client_store::bindings::{NewBinding, SyncMode, insert_binding};
    use vaultsyncd_domain::{ids::DeviceId, version::ServerEpoch};

    async fn fixture(label: &str) -> (std::path::PathBuf, Database, AgentConfig) {
        let path = std::env::temp_dir().join(format!(
            "s191-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let db = Database::open(path.join("client.sqlite")).await.unwrap();
        let config = serde_json::from_value(serde_json::json!({
            "server": {"id": "11".repeat(16), "url": "https://example.invalid"},
            "device_id": "22".repeat(16),
            "state_dir": path,
            "poll_millis": 500,
            "bindings": []
        }))
        .unwrap();
        (path, db, config)
    }

    async fn seed(db: &Database, id: u8, server: u8) -> BindingId {
        let binding = BindingId::from_bytes([id; 16]);
        let mut tx = db.begin_write().await.unwrap();
        insert_binding(
            &mut tx,
            &NewBinding {
                id: binding,
                server: ServerId::from_bytes([server; 16]),
                device: DeviceId::from_bytes([0x22; 16]),
                vault: VaultId::from_bytes([0x55; 16]),
                local_path: format!("C:/local/{binding}"),
                cache_path: format!("C:/cache/{binding}"),
                mode: SyncMode::Full,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        binding
    }

    fn problem(binding: BindingId) -> Problem {
        Problem {
            binding_id: binding.to_string(),
            code: "INSUFFICIENT_STORAGE".into(),
            entry_id: None,
            local_data: "retained".into(),
            next_step: "free space".into(),
            path: None,
            problem_id: "cycle".into(),
            reason: "storage unavailable".into(),
            retryable: false,
        }
    }

    async fn close(path: std::path::PathBuf, db: Database) {
        db.close().await;
        for _ in 0..100 {
            if std::fs::remove_dir_all(&path).is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("closed test database remains locked");
    }

    #[tokio::test]
    async fn one_binding_success_cannot_resolve_another_bindings_root_cause() {
        let (path, db, config) = fixture("aggregate").await;
        let a = seed(&db, 0x33, 0x11).await;
        let b = seed(&db, 0x44, 0x11).await;
        let mut cycles = HashMap::from([(a, problem(a)), (b, problem(b))]);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        let notice = crate::management_notices::list(&db)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(notice.state, "active");
        assert_eq!(notice.generation, "1");
        assert_eq!(
            notice.server_id,
            Some(ServerId::from_bytes([0x11; 16]).to_string())
        );
        cycles.remove(&a);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert_eq!(
            crate::management_notices::list(&db).await.unwrap()[0].state,
            "active"
        );
        cycles.clear();
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert_eq!(
            crate::management_notices::list(&db).await.unwrap()[0].state,
            "resolved"
        );
        let mut offline = problem(a);
        offline.code = "RATE_LIMITED".into();
        cycles.insert(a, offline);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert_eq!(crate::management_notices::list(&db).await.unwrap().len(), 1);
        cycles.insert(b, problem(b));
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert_eq!(
            crate::management_notices::list(&db).await.unwrap()[0].generation,
            "2"
        );
        close(path, db).await;
    }

    #[tokio::test]
    async fn equal_vault_ids_on_different_servers_keep_independent_causes_and_claims() {
        let (path, db, config) = fixture("server-scopes").await;
        let a = seed(&db, 0x33, 0x11).await;
        let b = seed(&db, 0x44, 0x66).await;
        let mut cycles = HashMap::from([(a, problem(a)), (b, problem(b))]);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        let notices = crate::management_notices::list(&db).await.unwrap();
        assert_eq!(notices.len(), 2);
        let first = notices
            .iter()
            .find(|notice| notice.server_id.as_deref() == Some(&"11".repeat(16)))
            .unwrap();
        let second = notices
            .iter()
            .find(|notice| notice.server_id.as_deref() == Some(&"66".repeat(16)))
            .unwrap();
        assert_ne!(first.key, second.key);
        assert!(
            crate::management_notices::claim(&db, &first.key, 1)
                .await
                .unwrap()
                .notification_claimed
                .unwrap()
        );
        assert!(
            !crate::management_notices::claim(&db, &first.key, 1)
                .await
                .unwrap()
                .notification_claimed
                .unwrap()
        );
        assert!(!second.claimed);
        let first_problems = crate::management::problems(&db, Some(a)).await.unwrap();
        let second_problems = crate::management::problems(&db, Some(b)).await.unwrap();
        assert_eq!(first_problems.len(), 1);
        assert_eq!(second_problems.len(), 1);
        assert!(first_problems[0].problem_id.contains(&first.key));
        assert!(second_problems[0].problem_id.contains(&second.key));

        cycles.remove(&a);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        let notices = crate::management_notices::list(&db).await.unwrap();
        let recovered = notices
            .iter()
            .find(|notice| notice.key == first.key)
            .unwrap();
        let active = notices
            .iter()
            .find(|notice| notice.key == second.key)
            .unwrap();
        assert_eq!(recovered.state, "resolved");
        assert!(recovered.claimed);
        assert_eq!(active.state, "active");
        assert_eq!(active.generation, "1");
        assert!(!active.claimed);
        cycles.insert(a, problem(a));
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert!(
            !crate::management_notices::claim(&db, &first.key, 1)
                .await
                .unwrap()
                .notification_claimed
                .unwrap()
        );
        assert!(
            crate::management_notices::claim(&db, &first.key, 2)
                .await
                .unwrap()
                .notification_claimed
                .unwrap()
        );
        assert!(
            crate::management_notices::claim(&db, &second.key, 1)
                .await
                .unwrap()
                .notification_claimed
                .unwrap()
        );
        close(path, db).await;
    }

    #[tokio::test]
    async fn unattributed_legacy_notice_retains_its_episode_and_claim() {
        let (path, db, config) = fixture("legacy-notice").await;
        seed(&db, 0x33, 0x11).await;
        let vault = VaultId::from_bytes([0x55; 16]);
        vaultsyncd_agent_core::status::issues::report(
            &db,
            vault,
            ErrorCode::InsufficientStorage,
            false,
        )
        .await
        .unwrap();
        let legacy = crate::management_notices::list(&db)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(legacy.server_id, None);
        assert_eq!(legacy.vault_id, Some(vault.to_string()));
        assert_eq!(legacy.cause, "INSUFFICIENT_STORAGE");
        assert!(
            crate::management_notices::claim(&db, &legacy.key, 1)
                .await
                .unwrap()
                .notification_claimed
                .unwrap()
        );
        sync(&db, &config, &HashMap::new(), &HashMap::new())
            .await
            .unwrap();
        let preserved = crate::management_notices::list(&db)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(preserved.key, legacy.key);
        assert_eq!(preserved.state, "active");
        assert_eq!(preserved.generation, "1");
        assert!(preserved.claimed);
        close(path, db).await;
    }
}
