//! S091本机IPC固定1.2开发fixture：改名后 major 1 被明确拒绝，当前 major 的旧 minor 仍可查询。
use vaultsyncd_local_ipc::{
    generated::{LocalIpcErrorCode, Request, ResponseKind},
    service::Service,
    version,
};
const OLD: &str = include_str!("../../../tests/contract/fixtures/s091/local-v1.2.json");
#[test]
fn s091_local_fixed_minor_compatibility_and_major_refusal() {
    let old: Request = serde_json::from_str(OLD).unwrap();
    let service = Service::new("S091-development-profile-not-formal-release");
    // 冻结的 1.2 fixture 是改名前的契约：major 不同即不兼容，必须明确拒绝。
    let refusal = service.handle(&old);
    assert_eq!(refusal.kind, ResponseKind::Error);
    assert_eq!(
        refusal.error.unwrap().code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    let mut current = old.clone();
    current.ipc_version = version::supported();
    current.ipc_version.minor = version::SUPPORTED_MINOR.saturating_sub(1);
    let response = service.handle(&current);
    assert_eq!(response.kind, ResponseKind::Ok);
    assert_eq!(response.request_id, current.request_id);
    let mut incompatible = old.clone();
    incompatible.ipc_version.major = version::SUPPORTED_MAJOR + 1;
    let response = service.handle(&incompatible);
    assert_eq!(response.kind, ResponseKind::Error);
    assert_eq!(
        response.error.unwrap().code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    assert!(response.result.is_none());
    let mut future = old;
    future.ipc_version.minor = version::SUPPORTED_MINOR + 1;
    assert_eq!(
        service.handle(&future).error.unwrap().code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    let mut unknown: serde_json::Value = serde_json::from_str(OLD).unwrap();
    unknown["command"]["name"] = "erase_all_local_data".into();
    assert!(serde_json::from_value::<Request>(unknown).is_err());
}
