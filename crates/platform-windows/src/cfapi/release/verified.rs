//! Formal S069 release under an owned continuous oplock, exact identity and full content proof.
//! This module performs no core/HTTP/database work. The application checks a live GC lease
//! immediately before `dehydrate`; holding only a historic clean flag never grants permission.
use crate::{
    cfapi::{
        catalog::Identity,
        raw::{CfDehydratePlaceholder, CfSetInSyncState},
    },
    files::{
        EntryMetadata, VaultHandle,
        controlled::{Evidence, OwnedProtection, ProtectedFile},
        native::metadata,
    },
};
use std::{
    io,
    os::windows::io::AsRawHandle,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use vaultsyncd_transfer::hash::ContentHasher;

#[link(name = "cldapi")]
unsafe extern "system" {
    fn CfSetPinState(
        handle: *mut std::ffi::c_void,
        state: i32,
        flags: u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}
fn failure(detail: impl std::fmt::Display) -> io::Error {
    io::Error::other(detail.to_string())
}
fn hr(v: i32) -> io::Result<()> {
    if v < 0 {
        Err(failure(format!("CFAPI release failed {v:08x}")))
    } else {
        Ok(())
    }
}
/// Continuous protected file, retained through the application's final residency transaction.
pub struct ReleaseHandle {
    guard: OwnedProtection,
    identity: Identity,
    generation: u64,
    size: u64,
    released_empty: AtomicBool,
}
impl ReleaseHandle {
    /// Pin all ancestors, forbid unknown reparses/hard links, and compare the real opened object.
    /// `empty_intent` is true only when a matching durable release intent exists for an empty file.
    pub fn open(
        vault: &VaultHandle,
        path: &str,
        id: Identity,
        generation: u64,
        size: u64,
        empty_intent: bool,
    ) -> io::Result<Self> {
        if generation == 0 {
            return Err(failure("UNKNOWN_LOCAL_STATE: zero generation"));
        }
        vault.with_parent(path, |_, _| {
            let before = vault.inspect(path)?;
            check(&before, id, size)?;
            let guard =
                ProtectedFile::open(&vault.vault_path().join(path.replace('/', "\\")), true)?
                    .into_owned_lease()?;
            let after = metadata(guard.file())?;
            check(&after, id, size)?;
            if before.identity != after.identity {
                return Err(failure("LOCAL_CHANGED: file identity"));
            }
            Ok(Self {
                guard,
                identity: id,
                generation,
                size,
                released_empty: AtomicBool::new(empty_intent),
            })
        })
    }
    /// No-recall metadata observation through the same protected file handle.
    pub fn metadata(&self) -> io::Result<EntryMetadata> {
        let m = metadata(self.guard.file())?;
        check(&m, self.identity, self.size)?;
        Ok(m)
    }
    /// Distinguish a complete released file from partial/unknown state without reading bytes.
    pub fn is_released(&self) -> io::Result<bool> {
        let m = self.metadata()?;
        let c = m.cloud.as_ref().expect("checked cloud");
        if c.on_disk == 0 && c.modified == 0 {
            return Ok(self.size > 0 || self.released_empty.load(Ordering::Acquire));
        }
        if c.on_disk < self.size {
            return Err(failure("CONTENT_UNAVAILABLE: partial content"));
        }
        Ok(false)
    }
    /// Stream a bounded real range while continuous protection prevents writes/deletion.
    /// Refuse after dehydration so inspection can never recall a released file.
    pub fn read_at(&self, offset: u64, length: u64) -> io::Result<Vec<u8>> {
        if self.is_released()?
            || length > 1024 * 1024
            || offset.checked_add(length).is_none_or(|end| end > self.size)
        {
            return Err(failure("CONTENT_UNAVAILABLE: invalid or released range"));
        }
        let mut b = vec![0; length as usize];
        let mut n = 0;
        while n < b.len() {
            let count = crate::files::controlled::read_at(
                self.guard.file(),
                &mut b[n..],
                offset + n as u64,
            )?;
            if count == 0 {
                return Err(failure("LOCAL_CHANGED: short read"));
            }
            n += count;
        }
        Ok(b)
    }
    /// Actual hash/physical identity proof, never inferred from in-sync or mtime.
    pub fn evidence(&self) -> io::Result<Evidence> {
        if self.is_released()? {
            return Err(failure("CONTENT_UNAVAILABLE: already released"));
        }
        let m = self.metadata()?;
        let mut hash = ContentHasher::new();
        let mut offset = 0;
        while offset < self.size {
            let bytes = self.read_at(offset, (self.size - offset).min(256 * 1024))?;
            offset += bytes.len() as u64;
            hash.update(&bytes);
        }
        let after = self.metadata()?;
        if m.identity != after.identity || m.size != after.size {
            return Err(failure("LOCAL_CHANGED"));
        }
        Ok(Evidence {
            generation: self.generation,
            volume: m.identity.volume,
            file: m.identity.file,
            digest: hash.finish(),
            size: self.size,
        })
    }
    /// Receipt identity/generation after a metadata-only check; never opens a content stream.
    pub fn released_identity(&self) -> io::Result<Option<(u64, Vec<u8>)>> {
        if !self.is_released()? {
            return Ok(None);
        }
        let m = self.metadata()?;
        let mut identity = m.identity.volume.to_le_bytes().to_vec();
        identity.extend(m.identity.file);
        Ok(Some((self.generation, identity)))
    }
    /// Release only after the caller has verified a current GC-protecting lease.
    /// Rehash under the same oplock; any CFAPI failure returns Err and preserves honest state.
    pub fn dehydrate(&self, expected: &Evidence) -> io::Result<()> {
        if self.evidence()? != *expected {
            return Err(failure("LOCAL_CHANGED: release evidence"));
        }
        let before = self.metadata()?;
        let prior = before.cloud.as_ref().expect("checked cloud").pin_state;
        let handle = self.guard.file().as_raw_handle();
        // An explicit manual request may relinquish a prior pinned/user-retained copy.
        hr(unsafe { CfSetPinState(handle, 2, 0, std::ptr::null_mut()) })?;
        let result = (|| {
            hr(unsafe { CfSetInSyncState(handle, 1, 0, std::ptr::null_mut()) })?;
            hr(unsafe {
                CfDehydratePlaceholder(
                    handle,
                    0,
                    if self.size == 0 {
                        -1
                    } else {
                        i64::try_from(self.size).map_err(failure)?
                    },
                    0,
                    std::ptr::null_mut(),
                )
            })?;
            self.guard.file().sync_all()?;
            self.released_empty.store(true, Ordering::Release);
            if !self.is_released()? {
                return Err(failure("CFAPI did not confirm complete dehydration"));
            }
            Ok(())
        })();
        if result.is_err() {
            let restored = unsafe { CfSetPinState(handle, prior as i32, 0, std::ptr::null_mut()) };
            if restored < 0 {
                return Err(failure(format!(
                    "release failed; pin restore also failed {restored:08x}; inspect retained state"
                )));
            }
        }
        result
    }
}
fn check(m: &EntryMetadata, id: Identity, size: u64) -> io::Result<()> {
    if m.directory || !m.supported() || m.links != 1 || m.size != size {
        return Err(failure("UNKNOWN_LOCAL_STATE: unsafe release target"));
    }
    let c = m
        .cloud
        .as_ref()
        .ok_or_else(|| failure("UNKNOWN_LOCAL_STATE: ordinary file"))?;
    if Identity::decode(&c.identity).map_err(|e| failure(format!("identity {e:?}")))? != id {
        return Err(failure("VERSION_MISMATCH: placeholder identity"));
    }
    Ok(())
}
/// Open the safe vault used by release ports; provided for application assembly convenience.
pub fn open_vault(path: &Path) -> io::Result<VaultHandle> {
    VaultHandle::open(path)
}
