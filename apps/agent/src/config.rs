//! Bounded credential-free process configuration. Filesystem admission follows under pinned handles.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
};
use vaultsyncd_domain::ids::{BindingId, DeviceId, ServerId, VaultId};

/// One authenticated server endpoint and its explicit development trust additions.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Stable server identity; it scopes Credential Manager and existing bindings.
    pub id: String,
    /// HTTPS origin/base path. User information, query, and fragments are forbidden.
    pub url: String,
    /// Explicit development opt-in, limited to loopback HTTP endpoints.
    #[serde(default)]
    pub development_http: bool,
    /// Optional additional development CA; system trust remains enabled.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Last successful login, loaded from the private client database, never a password.
    #[serde(skip)]
    pub username: Option<String>,
}
/// A local binding must match its durable database record exactly on restart.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindingConfig {
    /// Stable binding identity.
    pub id: String,
    /// Authorized remote Vault identity.
    pub vault_id: String,
    /// Existing ordinary NTFS directory.
    pub path: PathBuf,
    /// Existing internal cache directory, physically disjoint from every binding.
    pub cache: PathBuf,
    /// `full` or `on_demand`; unknown modes never become defaults.
    pub mode: String,
}
/// Minimal multi-binding Windows process configuration. Credentials are always external.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Server transport and trust policy.
    pub server: ServerConfig,
    /// Stable installation identity, reused for all operation retries.
    pub device_id: String,
    /// Existing private state directory containing the database and writer lock.
    pub state_dir: PathBuf,
    /// Periodic reconciliation fallback, milliseconds, from 250 through 60,000.
    pub poll_millis: u64,
    /// Configured bindings; admission also checks physical overlaps before any synchronization.
    pub bindings: Vec<BindingConfig>,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid agent configuration")
}
fn windows_absolute(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    let value = value.strip_prefix("\\\\?\\").unwrap_or(value).as_bytes();
    value.len() > 3
        && value[0].is_ascii_alphabetic()
        && value[1] == b':'
        && matches!(value[2], b'\\' | b'/')
        && !value.contains(&0)
}
impl AgentConfig {
    /// Read at most 1 MiB. Unknown keys (including plaintext credential fields) are errors.
    pub fn load(path: &Path) -> io::Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid());
        }
        Self::parse(&bytes)
    }
    /// Pure syntax, identity, transport, and bound validation; does not open user files.
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let value: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        value.validate()?;
        Ok(value)
    }
    /// Fail before acquiring database access when any supplied setting is unsupported.
    pub fn validate(&self) -> io::Result<()> {
        if ServerId::from_hex(&self.server.id)
            .map_err(|_| invalid())?
            .to_string()
            != self.server.id
            || DeviceId::from_hex(&self.device_id)
                .map_err(|_| invalid())?
                .to_string()
                != self.device_id
        {
            return Err(invalid());
        }
        let url = url::Url::parse(&self.server.url).map_err(|_| invalid())?;
        let loopback = matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if !url.has_host()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http" && self.server.development_http && loopback))
        {
            return Err(invalid());
        }
        if !windows_absolute(&self.state_dir)
            || self
                .server
                .ca_file
                .as_ref()
                .is_some_and(|p| !windows_absolute(p))
            || !(250..=60000).contains(&self.poll_millis)
            || self.bindings.len() > 1024
        {
            return Err(invalid());
        }
        let mut ids = HashSet::new();
        for binding in &self.bindings {
            if BindingId::from_hex(&binding.id)
                .map_err(|_| invalid())?
                .to_string()
                != binding.id
                || VaultId::from_hex(&binding.vault_id)
                    .map_err(|_| invalid())?
                    .to_string()
                    != binding.vault_id
                || !ids.insert(&binding.id)
                || !matches!(binding.mode.as_str(), "full" | "on_demand")
                || !windows_absolute(&binding.path)
                || !windows_absolute(&binding.cache)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> serde_json::Value {
        serde_json::json!({"server":{"id":"11111111111111111111111111111111","url":"https://sync.example"},"device_id":"22222222222222222222222222222222","state_dir":"C:\\state","poll_millis":1000,"bindings":[{"id":"33333333333333333333333333333333","vault_id":"44444444444444444444444444444444","path":"C:\\vault","cache":"C:\\cache","mode":"full"}]})
    }
    #[test]
    fn config_rejects_credentials_and_unsupported_transport() {
        let mut v = sample();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_ok());
        v["server"]["password"] = "secret".into();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_err());
        v = sample();
        v["server"]["url"] = "http://remote.example".into();
        v["server"]["development_http"] = true.into();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_err());
        v["server"]["url"] = "http://127.0.0.1:18770".into();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_ok());
        v["server"]["url"] = "https://user:password@sync.example".into();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    #[test]
    fn config_rejects_duplicate_bindings_and_relative_state() {
        let mut v = sample();
        let first = v["bindings"][0].clone();
        v["bindings"].as_array_mut().unwrap().push(first);
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_err());
        v = sample();
        v["state_dir"] = "relative".into();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_err());
        v = sample();
        v["bindings"][0]["mode"] = "readonly".into();
        assert!(AgentConfig::parse(&serde_json::to_vec(&v).unwrap()).is_err());
    }
}
