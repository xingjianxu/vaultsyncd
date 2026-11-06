//! Aggregate all bindings before resolving a Root cause. Notification records are not their own cause.
use crate::{config::AgentConfig, local::error};
use std::collections::{HashMap, HashSet};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{BindingId, RootId},
};
use vaultsyncd_local_ipc::generated::Problem;
use vaultsyncd_transfer::ports::PortError;
pub(crate) async fn sync(
    db: &Database,
    config: &AgentConfig,
    startup: &HashMap<BindingId, Problem>,
    cycles: &HashMap<BindingId, Problem>,
) -> Result<Vec<Problem>, PortError> {
    let mut origins = crate::management::problems(db, None).await?;
    let conflicts:Vec<(Vec<u8>,Vec<u8>,i64)>=sqlx::query_as("SELECT c.binding_id,b.root_id,c.seq FROM received_changes c JOIN bindings b ON b.id=c.binding_id WHERE EXISTS(SELECT 1 FROM json_each(c.events_json) e WHERE json_extract(e.value,'$.kind')='conflict')")
        .fetch_all(db.pool()).await.map_err(error)?;
    for (binding, root, seq) in conflicts {
        let root = RootId::from_bytes(
            root.try_into()
                .map_err(|_| error("invalid conflict root"))?,
        );
        let marker =
            serde_json::json!({"kind":"conflict_notice","root":root.to_string(),"seq":seq})
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
                &vaultsyncd_agent_core::status::issues::conflict_problem(root),
            )
            .await
            .map_err(error)?;
        }
        tx.commit().await.map_err(error)?;
    }
    origins.retain(|p| !p.problem_id.starts_with("notification:"));
    origins.extend(startup.values().chain(cycles.values()).cloned());
    let mut active = HashSet::new();
    let mut roots = HashSet::new();
    for binding in &config.bindings {
        roots.insert(RootId::from_hex(&binding.root_id).map_err(error)?);
    }
    for problem in &origins {
        if let Some(code) = ErrorCode::from_code(&problem.code).filter(|c| c.class().is_severe())
            && let Some(binding) = config.bindings.iter().find(|b| b.id == problem.binding_id)
        {
            active.insert((RootId::from_hex(&binding.root_id).map_err(error)?, code));
        }
    }
    for (root, code) in &active {
        vaultsyncd_agent_core::status::issues::report(db, *root, *code, false).await?;
    }
    let notices = crate::management_notices::list(db).await?;
    for notice in notices {
        if notice.state != "active" || notice.cause == "CONFLICT_COPY" {
            continue;
        }
        if let Some(root) = notice
            .root_id
            .as_deref()
            .and_then(|r| RootId::from_hex(r).ok())
            && roots.contains(&root)
            && let Some(code) = ErrorCode::from_code(&notice.cause)
            && !active.contains(&(root, code))
        {
            vaultsyncd_agent_core::status::issues::resolve_root(db, root, code).await?;
        }
    }
    Ok(startup.values().chain(cycles.values()).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn one_binding_success_cannot_resolve_another_bindings_root_cause() {
        let path = std::env::temp_dir().join(format!(
            "s083-aggregate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let db = Database::open(path.join("client.sqlite")).await.unwrap();
        let config:AgentConfig=serde_json::from_value(serde_json::json!({"server":{"id":"11".repeat(16),"url":"https://example.invalid"},"device_id":"22".repeat(16),"state_dir":"C:/test/state","poll_millis":500,"bindings":[{"id":"33".repeat(16),"root_id":"55".repeat(16),"path":"C:/a","cache":"C:/ca","mode":"full"},{"id":"44".repeat(16),"root_id":"55".repeat(16),"path":"C:/b","cache":"C:/cb","mode":"full"}]})).unwrap();
        let make = |binding_id: String| Problem {
            binding_id,
            code: "INSUFFICIENT_STORAGE".into(),
            entry_id: None,
            local_data: "retained".into(),
            next_step: "free space".into(),
            path: None,
            problem_id: "cycle".into(),
            reason: "storage unavailable".into(),
            retryable: false,
        };
        let a = BindingId::from_bytes([0x33; 16]);
        let b = BindingId::from_bytes([0x44; 16]);
        let mut cycles = HashMap::from([(a, make(a.to_string())), (b, make(b.to_string()))]);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        let n = crate::management_notices::list(&db)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(n.state, "active");
        assert_eq!(n.generation, "1");
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
        let mut offline = make(a.to_string());
        offline.code = "RATE_LIMITED".into();
        cycles.insert(a, offline);
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert_eq!(crate::management_notices::list(&db).await.unwrap().len(), 1);
        cycles.insert(b, make(b.to_string()));
        sync(&db, &config, &HashMap::new(), &cycles).await.unwrap();
        assert_eq!(
            crate::management_notices::list(&db).await.unwrap()[0].generation,
            "2"
        );
        db.close().await;
        for _ in 0..100 {
            if std::fs::remove_dir_all(&path).is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("closed test database remains locked");
    }
}
