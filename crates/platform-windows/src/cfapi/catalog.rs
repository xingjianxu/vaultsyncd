//! Persistent identity format and per-binding callback validation. No paths or secrets in blobs.
use std::collections::HashMap;
use vaultsyncd_domain::ids::{BindingId, EntryId, VaultId, VersionId};

/// Version 2 carries full domain IDs; prototype version 1 is deliberately not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Local binding, stable across process restarts.
    pub binding: BindingId,
    /// Remote vault.
    pub vault: VaultId,
    /// Stable entry, independent of its path.
    pub entry: EntryId,
    /// Fixed content version; never an implicit head lookup.
    pub version: VersionId,
}
impl Identity {
    /// Fixed encoded length, bounded below CFAPI's 4 KiB maximum.
    pub const LEN: usize = 72;
    /// Encode all IDs with schema and CRC.
    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        b[..4].copy_from_slice(b"VS\x02\0");
        for (chunk, id) in b[4..68].chunks_exact_mut(16).zip([
            self.binding.as_bytes(),
            self.vault.as_bytes(),
            self.entry.as_bytes(),
            self.version.as_bytes(),
        ]) {
            chunk.copy_from_slice(id);
        }
        let crc = super::identity::crc32(&b[..68]);
        b[68..].copy_from_slice(&crc.to_le_bytes());
        b
    }
    /// Decode strictly. CRC is corruption detection, not authorization.
    pub fn decode(b: &[u8]) -> Result<Self, IdentityError> {
        if b.len() != Self::LEN
            || &b[..4] != b"VS\x02\0"
            || super::identity::crc32(&b[..68]).to_le_bytes() != b[68..]
        {
            return Err(IdentityError::Malformed);
        }
        Ok(Self {
            binding: BindingId::from_bytes(b[4..20].try_into().expect("length")),
            vault: VaultId::from_bytes(b[20..36].try_into().expect("length")),
            entry: EntryId::from_bytes(b[36..52].try_into().expect("length")),
            version: VersionId::from_bytes(b[52..68].try_into().expect("length")),
        })
    }
}
/// Callback identity rejection; never treated as an absent local file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// Bad length/schema/checksum, including prototype identities.
    Malformed,
    /// Binding or remote vault differs from the connected vault.
    WrongVault,
    /// Entry was not loaded from the persistent binding state.
    UnknownEntry,
    /// Blob refers to an older or unrecognized version.
    StaleVersion,
}
/// Callback catalog loaded from durable local state before connecting.
#[derive(Clone, Debug)]
pub struct Catalog {
    /// Binding owning the connection.
    pub binding: BindingId,
    /// Remote vault owning the connection.
    pub vault: VaultId,
    entries: HashMap<EntryId, VersionId>,
}
impl Catalog {
    /// Empty catalogs refuse every file callback until entries are installed.
    pub fn new(binding: BindingId, vault: VaultId) -> Self {
        Self {
            binding,
            vault,
            entries: HashMap::new(),
        }
    }
    /// Publish an identity only after the corresponding platform/state transition succeeds.
    pub fn insert(&mut self, identity: Identity) -> Result<(), IdentityError> {
        if identity.binding != self.binding || identity.vault != self.vault {
            return Err(IdentityError::WrongVault);
        }
        self.entries.insert(identity.entry, identity.version);
        Ok(())
    }
    /// Resolve a blob against this connection's durable expected state.
    pub fn resolve(&self, blob: &[u8]) -> Result<Identity, IdentityError> {
        let id = Identity::decode(blob)?;
        if id.binding != self.binding || id.vault != self.vault {
            return Err(IdentityError::WrongVault);
        }
        match self.entries.get(&id.entry) {
            None => Err(IdentityError::UnknownEntry),
            Some(v) if *v != id.version => Err(IdentityError::StaleVersion),
            Some(_) => Ok(id),
        }
    }
    /// Stable vault registration identity, never based on a path.
    pub fn vault_blob(&self) -> [u8; 32] {
        let mut b = [0; 32];
        b[..16].copy_from_slice(self.binding.as_bytes());
        b[16..].copy_from_slice(self.vault.as_bytes());
        b
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reload_stale_corrupt_foreign_and_unknown() {
        let id = Identity {
            binding: BindingId::from_bytes([1; 16]),
            vault: VaultId::from_bytes([2; 16]),
            entry: EntryId::from_bytes([3; 16]),
            version: VersionId::from_bytes([4; 16]),
        };
        let bytes = id.encode();
        let mut cat = Catalog::new(id.binding, id.vault);
        assert_eq!(cat.resolve(&bytes), Err(IdentityError::UnknownEntry));
        cat.insert(id).unwrap();
        assert_eq!(cat.resolve(&bytes), Ok(id));
        let mut restarted = Catalog::new(id.binding, id.vault);
        restarted.insert(Identity::decode(&bytes).unwrap()).unwrap();
        assert_eq!(restarted.resolve(&bytes), Ok(id));
        restarted
            .insert(Identity {
                version: VersionId::from_bytes([5; 16]),
                ..id
            })
            .unwrap();
        assert_eq!(restarted.resolve(&bytes), Err(IdentityError::StaleVersion));
        for n in 0..bytes.len() {
            let mut bad = bytes;
            bad[n] ^= 1;
            assert_eq!(Identity::decode(&bad), Err(IdentityError::Malformed));
        }
        assert_eq!(
            Catalog::new(BindingId::from_bytes([8; 16]), id.vault).resolve(&bytes),
            Err(IdentityError::WrongVault)
        );
        assert_eq!(Identity::decode(&[0; 48]), Err(IdentityError::Malformed));
    }
}
