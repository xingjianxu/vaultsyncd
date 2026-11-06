//! Agent assembly. S067 supplies hydration; process/configuration and startup recovery are S070.
#[cfg(windows)]
pub mod bootstrap;
#[cfg(windows)]
mod hydration;
#[cfg(all(test, windows))]
#[allow(unsafe_code)]
mod hydration_tests;

#[cfg(windows)]
pub mod release;
