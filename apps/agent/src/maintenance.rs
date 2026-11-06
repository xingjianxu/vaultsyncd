//! Installed maintenance coordinator. A durable barrier prevents ordinary startup during replacement.
use crate::{config::AgentConfig, local::error};
use vaultsyncd_transfer::ports::PortError;
/// Reject normal writers while an interrupted upgrade requires explicit recovery.
pub fn admit_startup(config: &AgentConfig) -> Result<(), PortError> {
    if config
        .state_dir
        .parent()
        .ok_or_else(|| error("profile absent"))?
        .join("maintenance.json")
        .try_exists()
        .map_err(error)?
    {
        return Err(PortError::rejected(
            "MAINTENANCE_REQUIRED",
            "maintenance barrier exists; recover using the retained maintenance candidate",
        ));
    }
    Ok(())
}
