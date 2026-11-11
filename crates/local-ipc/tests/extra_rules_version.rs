//! S196：额外规则语义必须以 major 隔离，不能把 2.x 的空清单解释为仅默认规则。
use vaultsyncd_local_ipc::{
    generated::{CommandName, IpcVersion, LocalIpcErrorCode, Request, ResponseKind},
    service::Service,
    version,
};

#[test]
fn extra_rules_contract_is_ipc_3_0() {
    assert_eq!(version::supported(), IpcVersion { major: 3, minor: 0 });
}

#[test]
fn previous_complete_rules_clients_are_rejected_before_command_execution() {
    let examples: Vec<Request> = serde_json::from_str(include_str!(
        "../../../api/examples/local-ipc.management.json"
    ))
    .unwrap();
    let service = Service::new("S196-contract-test");
    let rules_requests: Vec<_> = examples
        .into_iter()
        .filter(|request| {
            matches!(
                request.command.name,
                CommandName::GetStatus | CommandName::GetRules | CommandName::SetRules
            )
        })
        .collect();
    assert_eq!(rules_requests.len(), 3);
    for mut request in rules_requests {
        for minor in [0, 6, 11] {
            request.ipc_version = IpcVersion { major: 2, minor };
            assert!(!version::is_compatible(&request.ipc_version));
            let response = service.handle(&request);
            response.validate().unwrap();
            assert_eq!(response.request_id, request.request_id);
            assert_eq!(response.kind, ResponseKind::Error);
            assert!(response.result.is_none());
            let error = response.error.unwrap();
            assert_eq!(error.code, LocalIpcErrorCode::UnsupportedVersion);
            assert!(!error.retryable);
        }
    }
}
