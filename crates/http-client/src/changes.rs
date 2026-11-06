//! `changes` 拉取端点（S048/S057）。
//!
//! SSE 不是事件来源；它只是在上层收到 `SeqHint` 后促使再次调用本模块。每次请求都带明确的
//! `after` 游标，服务端返回的事务批次保持完整，重复拉取是安全的。

use vaultsyncd_protocol::generated::ChangeList;

use crate::client::{ApiClient, ApiRequest, AuthMode};
use crate::error::ClientError;
use crate::transport::Transport;

/// 拉取一个 Root 的有序事务批次页。
///
/// `after` 必须是规范十进制文本；`limit` 为 `None` 时使用服务端默认值。Root 身份由调用方
/// 传入已验证的 32 位小写十六进制文本，不能把路径或查询串混入该参数。
pub async fn list<T: Transport>(
    client: &ApiClient<T>,
    root: &str,
    after: &str,
    limit: Option<u32>,
) -> Result<ChangeList, ClientError> {
    validate_root(root)?;
    validate_decimal(after, "after")?;
    let mut request = ApiRequest::get(format!("/v1/roots/{root}/changes"))
        .with_auth(AuthMode::Bearer)
        .with_query_param("after", after)?;
    if let Some(limit) = limit {
        let text = limit.to_string();
        request = request.with_query_param("limit", text)?;
    }
    client.send_json(request).await
}

fn validate_root(root: &str) -> Result<(), ClientError> {
    if root.len() != 32
        || !root
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ClientError::invalid_request(
            "Root 身份必须是 32 位小写十六进制",
        ));
    }
    Ok(())
}

fn validate_decimal(value: &str, field: &str) -> Result<(), ClientError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ClientError::invalid_request(format!(
            "{field} 必须是规范十进制"
        )));
    }
    value
        .parse::<u64>()
        .map_err(|_| ClientError::invalid_request(format!("{field} 超出范围")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeTransport, issued, json_response};
    use crate::transport::TransportRequest;
    use crate::{ClientConfig, Credentials};
    use std::sync::Arc;

    fn response() -> ChangeList {
        ChangeList {
            batches: Vec::new(),
            has_more: false,
            head_seq: "0".into(),
            next_after: "0".into(),
            root_id: "0123456789abcdef0123456789abcdef".into(),
            server_epoch: "1".into(),
        }
    }

    #[tokio::test]
    async fn list_encodes_query_and_uses_bearer_auth() {
        let transport = Arc::new(FakeTransport::new(|request: TransportRequest| {
            Box::pin(async move {
                assert!(request.url().contains("after=7&limit=10"));
                assert_eq!(request.header("authorization"), Some("Bearer access"));
                Ok(json_response(200, &response()))
            })
        }));
        let client = ApiClient::with_transport_for_unit_tests(
            ClientConfig::new("https://vault.example"),
            transport,
        )
        .expect("客户端");
        client
            .auth()
            .install(Credentials::from_issued(&issued("access", "refresh")).expect("凭据"));
        let result = list(&client, "0123456789abcdef0123456789abcdef", "7", Some(10))
            .await
            .expect("拉取");
        assert_eq!(result, response());
    }

    #[test]
    fn malformed_root_or_cursor_is_rejected_before_network() {
        assert!(validate_root("ABC").is_err());
        assert!(validate_decimal("01", "after").is_err());
        assert!(validate_decimal("", "after").is_err());
    }
}
