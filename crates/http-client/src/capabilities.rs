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
        ["v1", "roots"] => Ok("roots"),
        ["v1", "admin", "roots", ..] => Ok("root_admin"),
        ["v1", "admin", "backup-snapshots", ..] => Ok("backup_snapshots"),
        ["v1", "roots", _, "snapshots", ..] => Ok("snapshots"),
        ["v1", "roots", _, "changes", ..] => Ok("changes"),
        ["v1", "roots", _, "events", ..] => Ok("events"),
        ["v1", "roots", _, "uploads", ..] => Ok("uploads"),
        ["v1", "roots", _, "operations", ..] => Ok("operations"),
        ["v1", "roots", _, "trash", ..] => Ok("trash"),
        ["v1", "roots", _, "entries", _, "history", ..] => Ok("history"),
        ["v1", "roots", _, "entries", _, "versions", _, "content"] => Ok("content"),
        ["v1", "roots", _, "entries", _, "versions", _, "leases", ..] => Ok("version_leases"),
        ["v1", "roots", _, "entries", _, "subtree"] => Ok("operations"),
        ["v1", "roots", _, "conflicts", ..] | ["v1", "roots", _, "recovery", ..] => Ok("history"),
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
