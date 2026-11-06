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
mod namespace;
#[cfg(windows)]
pub mod remote;
#[cfg(windows)]
pub mod runtime;
#[cfg(windows)]
mod shutdown;
#[cfg(windows)]
mod work;

#[cfg(windows)]
pub mod release;
