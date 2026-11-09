//! Atomic fixed-version leases exposed to authenticated clients (S069).
//! A caller holds BEGIN IMMEDIATE across version/object checks and lease insertion.
use sqlx::{Row, Sqlite, Transaction};
use vaultsyncd_domain::ids::{EntryId, ObjectLeaseId, UserId, VaultId, VersionId};

/// Server-controlled bounded lease duration, in seconds.
pub const TTL_SECONDS: i64 = 120;
/// Stable caller request; the same ID can never refer to different content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Lease/request identity.
    pub id: ObjectLeaseId,
    /// Authorized vault.
    pub vault: VaultId,
    /// Fixed entry.
    pub entry: EntryId,
    /// Fixed version, never head.
    pub version: VersionId,
    /// Expected whole-file SHA256.
    pub digest: [u8; 32],
    /// Expected size.
    pub size: u64,
}
/// Persistent lease result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// Original fixed input.
    pub request: Request,
    /// UTC Unix seconds; release does not extend this deadline.
    pub expires_at: i64,
    /// Explicit release completed.
    pub closed: bool,
}
/// Failure keeps the caller's local bytes intact.
#[derive(Debug)]
pub enum Error {
    /// Vault is not owned by this account.
    OutOfScope,
    /// Version/object missing, collectable, or content facts disagree.
    Gone,
    /// Request ID reused with different fixed input.
    Reused,
    /// Expired/released identities cannot silently reacquire protection.
    Expired,
    /// Database failure.
    Storage(sqlx::Error),
}
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

async fn authorize(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    vault: VaultId,
) -> Result<(), Error> {
    let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vaults WHERE id=? AND owner_id=?")
        .bind(vault.as_bytes().as_slice())
        .bind(owner.as_bytes().as_slice())
        .fetch_optional(&mut **tx)
        .await?;
    found.map(|_| ()).ok_or(Error::OutOfScope)
}
async fn existing(tx: &mut Transaction<'_, Sqlite>, r: &Request) -> Result<Option<Lease>, Error> {
    let row=sqlx::query("SELECT q.*,coalesce(l.expires_at,0) AS expiry FROM version_lease_requests q LEFT JOIN object_leases l ON l.id=q.id WHERE q.id=?")
        .bind(r.id.as_bytes().as_slice()).fetch_optional(&mut **tx).await?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<Vec<u8>, _>("vault_id") != r.vault.as_bytes()
        || row.get::<Vec<u8>, _>("entry_id") != r.entry.as_bytes()
        || row.get::<Vec<u8>, _>("version_id") != r.version.as_bytes()
        || row.get::<Vec<u8>, _>("object_hash") != r.digest
        || row.get::<i64, _>("size") as u64 != r.size
    {
        return Err(Error::Reused);
    }
    Ok(Some(Lease {
        request: r.clone(),
        expires_at: row.get("expiry"),
        closed: row.get::<i64, _>("closed") != 0,
    }))
}
async fn version_exists(tx: &mut Transaction<'_, Sqlite>, r: &Request) -> Result<(), Error> {
    let found:Option<i64>=sqlx::query_scalar("SELECT 1 FROM versions v JOIN objects o ON o.vault_id=v.vault_id AND o.hash=v.object_hash WHERE v.vault_id=? AND v.entry_id=? AND v.id=? AND v.object_hash=? AND v.size=? AND o.size=v.size AND o.gc_state='live'")
        .bind(r.vault.as_bytes().as_slice()).bind(r.entry.as_bytes().as_slice()).bind(r.version.as_bytes().as_slice())
        .bind(r.digest.as_slice()).bind(i64::try_from(r.size).map_err(|_|Error::Gone)?).fetch_optional(&mut **tx).await?;
    found.map(|_| ()).ok_or(Error::Gone)
}
/// Establish protection atomically, or return the same unexpired lease on replay.
pub async fn acquire(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    r: &Request,
    now: i64,
) -> Result<Lease, Error> {
    authorize(tx, owner, r.vault).await?;
    if let Some(lease) = existing(tx, r).await? {
        if lease.closed || lease.expires_at <= now {
            return Err(Error::Expired);
        }
        version_exists(tx, r).await?;
        return Ok(lease);
    }
    version_exists(tx, r).await?;
    let expiry = now
        .checked_add(TTL_SECONDS)
        .filter(|_| now >= 0)
        .ok_or(Error::Gone)?;
    sqlx::query("INSERT INTO object_leases(id,vault_id,object_hash,purpose,expires_at) VALUES(?,?,?,'download',?)")
        .bind(r.id.as_bytes().as_slice()).bind(r.vault.as_bytes().as_slice()).bind(r.digest.as_slice()).bind(expiry).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO version_lease_requests(id,vault_id,entry_id,version_id,object_hash,size) VALUES(?,?,?,?,?,?)")
        .bind(r.id.as_bytes().as_slice()).bind(r.vault.as_bytes().as_slice()).bind(r.entry.as_bytes().as_slice())
        .bind(r.version.as_bytes().as_slice()).bind(r.digest.as_slice()).bind(r.size as i64).execute(&mut **tx).await?;
    Ok(Lease {
        request: r.clone(),
        expires_at: expiry,
        closed: false,
    })
}
/// Inspect/renew an existing exact version lease. Expiry never resurrects an old request.
pub async fn inspect(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    r: &Request,
    now: i64,
    renew: bool,
) -> Result<Lease, Error> {
    authorize(tx, owner, r.vault).await?;
    let mut lease = existing(tx, r).await?.ok_or(Error::Expired)?;
    if lease.closed || lease.expires_at <= now {
        return Err(Error::Expired);
    }
    version_exists(tx, r).await?;
    if renew {
        lease.expires_at = now.checked_add(TTL_SECONDS).ok_or(Error::Gone)?;
        sqlx::query("UPDATE object_leases SET expires_at=? WHERE id=?")
            .bind(lease.expires_at)
            .bind(r.id.as_bytes().as_slice())
            .execute(&mut **tx)
            .await?;
    }
    Ok(lease)
}
/// Idempotent explicit release, retaining fixed request identity for later replay rejection.
pub async fn release(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    r: &Request,
) -> Result<Lease, Error> {
    authorize(tx, owner, r.vault).await?;
    let mut lease = existing(tx, r).await?.ok_or(Error::Expired)?;
    sqlx::query("DELETE FROM object_leases WHERE id=?")
        .bind(r.id.as_bytes().as_slice())
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE version_lease_requests SET closed=1 WHERE id=?")
        .bind(r.id.as_bytes().as_slice())
        .execute(&mut **tx)
        .await?;
    lease.closed = true;
    lease.expires_at = 0;
    Ok(lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::{changes, leases, test_support::*, versions};
    async fn seed(label: &str) -> (TestDir, Database, UserId, Request, i64) {
        let dir = TestDir::new(label);
        let db = open_database(&dir).await;
        let owner = seed_owner(&db, 1).await;
        let (vault, parent) = register_vault(&db, owner, 2).await;
        let entry = seed_file(&db, owner, vault, parent, 3, "content").await;
        let digest = seed_object(&db, owner, vault, 4, 12).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let version = VersionId::from_bytes([5; 16]);
        let mut tx = db.begin_write().await.unwrap();
        let batch = changes::append_change(
            &mut tx,
            owner,
            vault,
            crate::test_support::tx(6),
            "[{\"kind\":\"test\"}]",
            at(now),
        )
        .await
        .unwrap();
        versions::record_version(
            &mut tx,
            owner,
            &versions::NewVersion {
                id: version,
                vault,
                entry,
                object_hash: digest,
                size: 12,
                created_seq: batch.seq,
                origin: versions::VersionOrigin::Client,
                at: at(now),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (
            dir,
            db,
            owner,
            Request {
                id: object_lease(7),
                vault,
                entry,
                version,
                digest,
                size: 12,
            },
            now,
        )
    }
    #[tokio::test]
    async fn exact_pin_replay_renew_release_expiry_and_gc_protection() {
        let (_dir, db, owner, r, now) = seed("version-lease-gc").await;
        let mut tx = db.begin_write().await.unwrap();
        let l = acquire(&mut tx, owner, &r, now).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(l.expires_at, now + TTL_SECONDS);
        let mut tx = db.begin_write().await.unwrap();
        assert_eq!(acquire(&mut tx, owner, &r, now + 10).await.unwrap(), l);
        let active = leases::active_object_leases(&mut tx, owner, r.vault, at(now))
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        let attempted =
            sqlx::query("UPDATE objects SET gc_state='collectable' WHERE vault_id=? AND hash=?")
                .bind(r.vault.as_bytes().as_slice())
                .bind(r.digest.as_slice())
                .execute(&mut *tx)
                .await;
        assert!(attempted.is_err(), "GC cannot mark a protected object");
        assert_eq!(
            inspect(&mut tx, owner, &r, now + 11, true)
                .await
                .unwrap()
                .expires_at,
            now + 11 + TTL_SECONDS
        );
        let mut bad = r.clone();
        bad.version = VersionId::from_bytes([8; 16]);
        assert!(matches!(
            acquire(&mut tx, owner, &bad, now + 12).await,
            Err(Error::Reused)
        ));
        assert!(matches!(
            acquire(&mut tx, user(9), &r, now).await,
            Err(Error::OutOfScope)
        ));
        assert!(matches!(
            inspect(&mut tx, owner, &r, now + 500, true).await,
            Err(Error::Expired)
        ));
        assert!(release(&mut tx, owner, &r).await.unwrap().closed);
        assert!(release(&mut tx, owner, &r).await.unwrap().closed);
        assert!(matches!(
            acquire(&mut tx, owner, &r, now + 12).await,
            Err(Error::Expired)
        ));
        assert!(
            leases::active_object_leases(&mut tx, owner, r.vault, at(now))
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE objects SET gc_state='collectable' WHERE vault_id=? AND hash=?")
            .bind(r.vault.as_bytes().as_slice())
            .bind(r.digest.as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        db.close().await;
    }
    #[tokio::test]
    async fn lost_response_restart_and_missing_or_wrong_version_never_fall_back_to_head() {
        let (dir, db, owner, r, now) = seed("version-lease-restart").await;
        let mut tx = db.begin_write().await.unwrap();
        let first = acquire(&mut tx, owner, &r, now).await.unwrap();
        tx.commit().await.unwrap();
        db.close().await;
        let db = Database::open(dir.database()).await.unwrap();
        let mut tx = db.begin_write().await.unwrap();
        assert_eq!(acquire(&mut tx, owner, &r, now + 1).await.unwrap(), first);
        let mut missing = r.clone();
        missing.id = object_lease(10);
        missing.version = VersionId::from_bytes([11; 16]);
        assert!(matches!(
            acquire(&mut tx, owner, &missing, now).await,
            Err(Error::Gone)
        ));
        missing.version = r.version;
        missing.size += 1;
        assert!(matches!(
            acquire(&mut tx, owner, &missing, now).await,
            Err(Error::Gone)
        ));
        tx.rollback().await.unwrap();
        db.close().await;
    }
}
