//! Active path reservations are separate from durable, detached recovery records.
use crate::{config::AgentConfig, local::error, management::reject};
use std::{collections::HashSet, io, path::Path};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::{BindingId, VaultId};
use vaultsyncd_platform_windows::{
    cfapi::{catalog::Catalog, maintenance},
    files::VaultHandle,
};
use vaultsyncd_transfer::ports::PortError;

pub(crate) async fn detached(db: &Database) -> Result<HashSet<String>, PortError> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT lower(hex(binding_id)) FROM binding_controls WHERE detached=1")
            .fetch_all(db.pool())
            .await
            .map_err(error)?;
    Ok(ids.into_iter().collect())
}

fn detached_root(path: &Path) -> Result<Option<VaultHandle>, PortError> {
    match VaultHandle::open(path) {
        Ok(root) => Ok(Some(root)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(reject("LOCAL_PATH_UNAVAILABLE")),
    }
}

pub(crate) async fn admit(db: &Database, config: &AgentConfig) -> Result<(), PortError> {
    config.validate().map_err(error)?;
    let detached = detached(db).await?;
    // Historical caches still contain immutable snapshots and recovery journals. Never reuse them.
    let mut internal = vec![VaultHandle::open(&config.state_dir).map_err(error)?];
    for binding in &config.bindings {
        let cache =
            VaultHandle::open(&binding.cache).map_err(|_| reject("LOCAL_PATH_UNAVAILABLE"))?;
        if internal.iter().any(|old| old.overlaps(&cache)) {
            return Err(reject("PATH_OVERLAP"));
        }
        internal.push(cache);
    }
    let mut roots = Vec::new();
    for binding in &config.bindings {
        let archived = detached.contains(&binding.id);
        let root = if archived {
            let Some(root) = detached_root(&binding.path)? else {
                continue;
            };
            root
        } else {
            VaultHandle::open(&binding.path).map_err(|_| reject("LOCAL_PATH_UNAVAILABLE"))?
        };
        if internal.iter().any(|old| old.overlaps(&root)) {
            return Err(reject("PATH_OVERLAP"));
        }
        roots.push((root, archived));
    }
    for (i, (root, archived)) in roots.iter().enumerate() {
        for (old, old_archived) in &roots[..i] {
            if root.overlaps(old)
                && !(*archived && *old_archived)
                // Only the exact physical root can be handed over. Nested old CFAPI roots
                // and partial children must not silently become part of another binding.
                && !(root.identity() == old.identity() && (*archived || *old_archived))
            {
                return Err(reject("PATH_OVERLAP"));
            }
        }
    }
    Ok(())
}

/// Provider removal follows actual registration ownership, not whether a row is detached.
/// A handed-over recovery record must never unregister the new owner's root.
pub(crate) async fn provider_bindings(
    db: &Database,
    config: &AgentConfig,
) -> Result<Vec<crate::config::BindingConfig>, PortError> {
    let detached = detached(db).await?;
    let mut result = Vec::new();
    for binding in &config.bindings {
        let Some(root) = detached_root(&binding.path)? else {
            if detached.contains(&binding.id) {
                continue;
            }
            return Err(reject("LOCAL_PATH_UNAVAILABLE"));
        };
        let expected = Catalog::new(
            BindingId::from_hex(&binding.id).map_err(error)?,
            VaultId::from_hex(&binding.vault_id).map_err(error)?,
        )
        .vault_blob();
        match maintenance::registered_identity(&binding.path).map_err(error)? {
            Some(identity) if identity != expected => {
                let mut successor = false;
                for other in &config.bindings {
                    if Catalog::new(
                        BindingId::from_hex(&other.id).map_err(error)?,
                        VaultId::from_hex(&other.vault_id).map_err(error)?,
                    )
                    .vault_blob()
                        == identity
                        && let Some(other_root) = detached_root(&other.path)?
                        && other_root.identity() == root.identity()
                    {
                        successor = true;
                        break;
                    }
                }
                if !detached.contains(&binding.id) || !successor {
                    return Err(reject("BINDING_BLOCKED"));
                }
            }
            Some(_) => result.push(binding.clone()),
            None => {
                let id = BindingId::from_hex(&binding.id).map_err(error)?;
                if !detached.contains(&binding.id)
                    || vaultsyncd_platform_windows::shell::registered(id).map_err(error)?
                {
                    result.push(binding.clone());
                }
            }
        }
    }
    Ok(result)
}

/// Preflight before any binding row or native recovery is created. Only complete old content
/// can change provider identity; unregistering a partial placeholder could delete its data.
pub(crate) async fn prepare_reuse(
    db: &Database,
    config: &AgentConfig,
    path: &Path,
) -> Result<(), PortError> {
    let detached = detached(db).await?;
    let root = VaultHandle::open(path).map_err(|_| reject("LOCAL_PATH_UNAVAILABLE"))?;
    let mut previous = Vec::new();
    for binding in &config.bindings {
        if detached.contains(&binding.id)
            && let Some(old) = detached_root(&binding.path)?
            && old.identity() == root.identity()
        {
            previous.push(binding);
        }
    }
    if previous.is_empty() {
        return Ok(());
    }
    let registration = maintenance::registered_identity(path).map_err(error)?;
    if let Some(identity) = registration
        && !previous.iter().any(|binding| {
            let (Ok(id), Ok(vault)) = (
                BindingId::from_hex(&binding.id),
                VaultId::from_hex(&binding.vault_id),
            ) else {
                return false;
            };
            Catalog::new(id, vault).vault_blob() == identity
        })
    {
        return Err(reject("BINDING_BLOCKED"));
    }
    // Scan first, without hydration or content mutation. A failure never leaves a new binding.
    maintenance::check_materialized(path).map_err(|_| reject("MODE_REQUIRES_LOCAL_COPY"))?;
    let report = root.scan();
    if !report.permits_missing_comparison() {
        return Err(reject("LOCAL_PATH_UNAVAILABLE"));
    }
    for entry in report.entries {
        if let Some(cloud) = entry.metadata.and_then(|meta| meta.cloud) {
            let identity =
                vaultsyncd_platform_windows::cfapi::catalog::Identity::decode(&cloud.identity)
                    .map_err(error)?;
            if !previous.iter().any(|binding| {
                binding.id == identity.binding.to_string()
                    && binding.vault_id == identity.vault.to_string()
            }) {
                return Err(reject("BINDING_BLOCKED"));
            }
        }
    }
    root.verify().map_err(error)?;
    // Existing lossless conversion pins each entry, checks physical identity and verifies hashes.
    maintenance::revert_materialized(path).map_err(error)?;
    if registration.is_some() {
        maintenance::unregister_materialized(path).map_err(error)?;
    }
    for binding in previous {
        let id = BindingId::from_hex(&binding.id).map_err(error)?;
        if vaultsyncd_platform_windows::shell::registered(id).map_err(error)? {
            vaultsyncd_platform_windows::shell::unregister(id).map_err(error)?;
        }
    }
    root.verify().map_err(error)
}
