//! Exact fixed-version content leases; a successful ordinary GET is never lease evidence.
use crate::{
    ApiClient, ApiRequest, AuthMode,
    transport::Transport,
    uploads::{is_identity, map_error},
};
use vaultsyncd_protocol::generated::{VersionLease, VersionLeaseRequest};
use vaultsyncd_transfer::{
    hash,
    ports::{PortError, VersionPin},
};

/// Authenticated endpoint bound to one remote Root.
pub struct LeaseEndpoint<T: Transport> {
    client: ApiClient<T>,
    root: String,
}
impl<T: Transport> Clone for LeaseEndpoint<T> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            root: self.root.clone(),
        }
    }
}
impl<T: Transport> LeaseEndpoint<T> {
    /// Bind an authorized Root. Invalid identities fail before sending HTTP.
    pub fn new(client: ApiClient<T>, root: impl Into<String>) -> Result<Self, PortError> {
        let root = root.into();
        if !is_identity(&root) {
            return Err(PortError::protocol("invalid lease root"));
        }
        Ok(Self { client, root })
    }
    /// Establish a GC-protecting lease. Generate the ID once and reuse it after ambiguous responses.
    pub async fn acquire(&self, pin: &VersionPin, id: &str) -> Result<VersionLease, PortError> {
        self.call(pin, id, "").await
    }
    /// Renew only this exact unexpired lease; errors never authorize dehydration.
    pub async fn renew(&self, pin: &VersionPin, id: &str) -> Result<VersionLease, PortError> {
        self.call(pin, id, "renew").await
    }
    /// Explicit idempotent release; otherwise the bounded server TTL eventually expires.
    pub async fn release(&self, pin: &VersionPin, id: &str) -> Result<VersionLease, PortError> {
        self.call(pin, id, "release").await
    }
    async fn call(
        &self,
        pin: &VersionPin,
        id: &str,
        action: &str,
    ) -> Result<VersionLease, PortError> {
        if !is_identity(&pin.entry) || !is_identity(&pin.version) || !is_identity(id) {
            return Err(PortError::protocol("invalid fixed lease identity"));
        }
        let body = VersionLeaseRequest {
            lease_id: id.to_owned(),
            sha256: hash::to_hex(&pin.digest),
            size: pin.size.to_string(),
        };
        let mut path = format!(
            "/v1/roots/{}/entries/{}/versions/{}/leases",
            self.root, pin.entry, pin.version
        );
        if !action.is_empty() {
            path.push_str(&format!("/{id}/{action}"));
        }
        let request = ApiRequest::post(path)
            .with_auth(AuthMode::Bearer)
            .with_json(&body)
            .map_err(map_error)?;
        let lease: VersionLease = self.client.send_json(request).await.map_err(map_error)?;
        let ttl = lease
            .ttl_seconds
            .parse::<u64>()
            .map_err(|_| PortError::protocol("invalid lease TTL"))?;
        let expiry = lease
            .expires_at
            .parse::<u64>()
            .map_err(|_| PortError::protocol("invalid lease expiry"))?;
        if lease.lease_id != id
            || lease.root_id != self.root
            || lease.entry_id != pin.entry
            || lease.version_id != pin.version
            || lease.sha256 != body.sha256
            || lease.size != body.size
            || ttl > 120
            || (action == "release") != lease.closed
            || (action != "release" && (ttl == 0 || expiry == 0))
        {
            return Err(PortError::protocol(
                "lease receipt does not match exact content or is inactive",
            ));
        }
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClientConfig, Credentials,
        testing::{FakeTransport, issued, json_response},
    };
    use std::sync::Arc;
    fn pin() -> VersionPin {
        VersionPin::new(
            "binding",
            "33333333333333333333333333333333",
            "44444444444444444444444444444444",
            [5; 32],
            12,
        )
        .unwrap()
    }
    const ROOT: &str = "22222222222222222222222222222222";
    const ID: &str = "66666666666666666666666666666666";
    fn receipt() -> VersionLease {
        VersionLease {
            lease_id: ID.into(),
            root_id: ROOT.into(),
            entry_id: pin().entry,
            version_id: pin().version,
            sha256: hash::to_hex(&[5; 32]),
            size: "12".into(),
            expires_at: "2000".into(),
            ttl_seconds: "120".into(),
            closed: false,
        }
    }
    fn endpoint(v: VersionLease) -> (LeaseEndpoint<FakeTransport>, Arc<FakeTransport>) {
        let transport = Arc::new(FakeTransport::new(move |_| {
            let response = json_response(200, &v);
            Box::pin(async move { Ok(response) })
        }));
        let client = ApiClient::with_transport(
            ClientConfig::new("https://vault.example"),
            transport.clone(),
        )
        .unwrap();
        client
            .auth()
            .install(Credentials::from_issued(&issued("a", "r")).unwrap());
        (LeaseEndpoint::new(client, ROOT).unwrap(), transport)
    }
    #[tokio::test]
    async fn exact_receipt_required_and_invalid_ids_never_send() {
        let (e, t) = endpoint(receipt());
        assert!(e.acquire(&pin(), "../bad").await.is_err());
        assert_eq!(t.call_count(), 0);
        assert!(e.acquire(&pin(), ID).await.is_ok());
        assert!(
            t.calls()[0]
                .url()
                .ends_with("/versions/44444444444444444444444444444444/leases")
        );
        for field in 0..9 {
            let mut bad = receipt();
            match field {
                0 => bad.lease_id = "7".repeat(32),
                1 => bad.root_id = "7".repeat(32),
                2 => bad.entry_id = "7".repeat(32),
                3 => bad.version_id = "7".repeat(32),
                4 => bad.sha256 = "7".repeat(64),
                5 => bad.size = "13".into(),
                6 => bad.ttl_seconds = "0".into(),
                7 => bad.ttl_seconds = "121".into(),
                _ => bad.closed = true,
            }
            let (e, _) = endpoint(bad);
            assert!(e.acquire(&pin(), ID).await.is_err(), "field {field}");
        }
    }
    #[tokio::test]
    async fn renew_cannot_accept_closed_receipt_and_release_requires_closed() {
        let (e, _) = endpoint(receipt());
        assert!(e.renew(&pin(), ID).await.is_ok());
        assert!(e.release(&pin(), ID).await.is_err());
        let mut closed = receipt();
        closed.closed = true;
        closed.ttl_seconds = "0".into();
        closed.expires_at = "0".into();
        let (e, t) = endpoint(closed);
        assert!(e.release(&pin(), ID).await.is_ok());
        assert!(e.renew(&pin(), ID).await.is_err());
        assert!(t.calls()[0].url().ends_with(&format!("/{ID}/release")));
    }
}
