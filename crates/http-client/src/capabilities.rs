//! 每个生产HTTP动作在发送凭据/内容前重新协商，不能把未知功能或minor当作默认授权。
use crate::{ClientError, ProtocolError};
use vaultsyncd_protocol::{generated::Capabilities, negotiation};
pub(crate) const PATH: &str = "/v1/capabilities";
pub(crate) fn feature(path: &str) -> Result<&'static str, ClientError> {
    if path.starts_with("/v1/auth/") {
        return Ok("auth");
    }
    let parts: Vec<_> = path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        ["v1", "vaults"] => Ok("vaults"),
        ["v1", "admin", "vaults", ..] => Ok("vault_admin"),
        ["v1", "admin", "backup-snapshots", ..] => Ok("backup_snapshots"),
        ["v1", "vaults", _, "snapshots", ..] => Ok("snapshots"),
        ["v1", "vaults", _, "changes", ..] => Ok("changes"),
        ["v1", "vaults", _, "events", ..] => Ok("events"),
        ["v1", "vaults", _, "uploads", ..] => Ok("uploads"),
        ["v1", "vaults", _, "operations", ..] => Ok("operations"),
        ["v1", "vaults", _, "trash", ..] => Ok("trash"),
        ["v1", "vaults", _, "activity"] => Ok("file_activity"),
        ["v1", "vaults", _, "entries", _, "history", ..] => Ok("history"),
        ["v1", "vaults", _, "entries", _, "versions", _, "content"] => Ok("content"),
        ["v1", "vaults", _, "entries", _, "versions", _, "leases", ..] => Ok("version_leases"),
        ["v1", "vaults", _, "entries", _, "subtree"] => Ok("operations"),
        ["v1", "vaults", _, "conflicts", ..] | ["v1", "vaults", _, "recovery", ..] => Ok("history"),
        _ => Err(ClientError::invalid_request(
            "endpoint has no known negotiated capability",
        )),
    }
}

pub(crate) fn admit(value: &Capabilities, path: &str) -> Result<(), ClientError> {
    negotiation::negotiate(value)
        .and_then(|n| n.require(feature(path).map_err(|_| negotiation::Error::Malformed)?))
        .map_err(|e| {
            ClientError::Protocol(ProtocolError::new(format!(
                "remote protocol negotiation rejected: {e}"
            )))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_requires_the_explicit_server_capability() {
        let path = "/v1/vaults/11111111111111111111111111111111/activity";
        assert_eq!(feature(path).unwrap(), "file_activity");
        let mut capabilities: Capabilities = serde_json::from_str(include_str!(
            "../../../api/examples/capabilities.response.json"
        ))
        .unwrap();
        capabilities.features.push("file_activity".into());
        admit(&capabilities, path).unwrap();
        capabilities
            .features
            .retain(|feature| feature != "file_activity");
        assert!(admit(&capabilities, path).is_err());
        assert!(feature(&format!("{path}/unknown")).is_err());
    }

    #[tokio::test]
    async fn wrong_server_never_receives_login_or_bearer_requests() {
        use crate::{
            ApiClient, ApiRequest, AuthMode, ClientConfig, Credentials,
            testing::{FakeTransport, json_response},
            transport::TransportRequest,
        };
        use std::sync::Arc;
        use vaultsyncd_domain::ids::ServerId;
        use vaultsyncd_protocol::generated::{LoginRequest, SessionIssued};

        let transport = Arc::new(FakeTransport::new(move |request: TransportRequest| {
            Box::pin(async move {
                assert!(request.url().ends_with("/v1/capabilities"));
                let value: Capabilities = serde_json::from_str(include_str!(
                    "../../../api/examples/capabilities.response.json"
                ))
                .unwrap();
                Ok(json_response(200, &value))
            })
        }));
        let client = ApiClient::with_transport(
            ClientConfig::new("https://vault.invalid"),
            transport.clone(),
        )
        .unwrap()
        .with_expected_server(ServerId::from_bytes([255; 16]))
        .clone();
        let cause = client
            .login(&LoginRequest {
                login: "user".into(),
                password: "test-password".into(),
                device_label: "test".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(cause, crate::ClientError::ServerIdentityMismatch));
        assert_eq!(crate::transfer_error(cause).code(), Some("NEEDS_RECOVERY"));
        client.auth().install(
            Credentials::from_issued(&SessionIssued {
                session_id: "11".repeat(16),
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                access_expires_at: "10000000000".into(),
                refresh_expires_at: "10000000001".into(),
            })
            .unwrap(),
        );
        assert!(
            client
                .send(ApiRequest::get("/v1/vaults").with_auth(AuthMode::Bearer))
                .await
                .is_err()
        );
        assert_eq!(transport.call_count(), 2);
        assert!(client.auth().has_credentials());
    }

    #[tokio::test]
    async fn unsupported_activity_does_not_send_the_business_request() {
        use crate::{
            ApiClient, ApiRequest, ClientConfig,
            testing::{FakeTransport, json_response},
            transport::TransportRequest,
        };
        use std::sync::Arc;
        for supported in [false, true] {
            let transport = Arc::new(FakeTransport::new(move |request: TransportRequest| {
                Box::pin(async move {
                    if request.url().ends_with("/v1/capabilities") {
                        let mut capabilities: Capabilities = serde_json::from_str(include_str!(
                            "../../../api/examples/capabilities.response.json"
                        ))
                        .unwrap();
                        capabilities
                            .features
                            .retain(|feature| feature != "file_activity");
                        if supported {
                            capabilities.features.push("file_activity".into());
                        }
                        Ok(json_response(200, &capabilities))
                    } else {
                        assert!(supported, "unsupported capability leaked business request");
                        assert!(request.url().ends_with("/activity"));
                        Ok(json_response(200, &serde_json::json!({"items":[]})))
                    }
                })
            }));
            let client = ApiClient::with_transport(
                ClientConfig::new("https://vault.invalid"),
                transport.clone(),
            )
            .unwrap();
            let result = client
                .send_json::<vaultsyncd_protocol::generated::ActivityPage>(ApiRequest::get(
                    "/v1/vaults/11111111111111111111111111111111/activity",
                ))
                .await;
            assert_eq!(result.is_ok(), supported);
            assert_eq!(transport.call_count(), if supported { 2 } else { 1 });
        }
    }
}
