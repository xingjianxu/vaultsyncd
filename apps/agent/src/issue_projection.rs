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
