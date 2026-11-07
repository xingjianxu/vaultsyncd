//! Agent assembly. S067 supplies hydration; process/configuration and startup recovery are S070.
#[cfg(windows)]
pub mod bootstrap;
pub mod config;
#[cfg(windows)]
mod conflicts;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod events;
#[cfg(windows)]
mod exclusions;
#[cfg(windows)]
mod hydration;
#[cfg(all(test, windows))]
#[allow(unsafe_code)]
mod hydration_tests;
#[cfg(windows)]
mod importer;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod issue_projection;
#[cfg(windows)]
pub mod local;
#[cfg(windows)]
mod management;
#[cfg(windows)]
mod management_export;
#[cfg(windows)]
mod management_files;
#[cfg(windows)]
mod management_history;
#[cfg(windows)]
mod management_listing;
#[cfg(windows)]
mod management_notices;
#[cfg(windows)]
mod namespace;
#[cfg(windows)]
pub mod remote;
#[cfg(windows)]
mod resync;
#[cfg(windows)]
pub mod runtime;
#[cfg(windows)]
mod shutdown;
#[cfg(windows)]
pub mod user_session;
#[cfg(windows)]
mod work;

#[cfg(windows)]
pub mod release;

#[cfg(windows)]
pub mod maintenance;
#[cfg(windows)]
mod maintenance_prepare;
#[cfg(windows)]
pub mod portable;
