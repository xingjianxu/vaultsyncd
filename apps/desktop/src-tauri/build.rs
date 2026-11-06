//! Build Windows resources and deny custom commands unless explicitly permitted.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        tauri_build::try_build(
            tauri_build::Attributes::new()
                .app_manifest(tauri_build::AppManifest::new().commands(&["agent_get_status"])),
        )
        .expect("valid desktop configuration");
    }
}
