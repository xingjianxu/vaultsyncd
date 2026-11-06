use super::*;
use crate::status::tests::{B, Dir, setup};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
fn io(e: impl std::fmt::Display) -> PortError {
    error(e)
}
struct Source {
    root: PathBuf,
    changed: AtomicBool,
    mutate: bool,
    late: bool,
    calls: std::sync::atomic::AtomicUsize,
}
struct Data {
    file: Mutex<std::fs::File>,
    identity: String,
    size: u64,
}
impl SnapshotSource for Data {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn total_size(&self) -> u64 {
        self.size
    }
    fn read(&self, o: u64, l: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let mut f = self.file.lock().unwrap();
            f.seek(SeekFrom::Start(o)).map_err(io)?;
            let mut bytes = vec![0; l as usize];
            f.read_exact(&mut bytes).map_err(io)?;
            Ok(bytes)
        })
    }
}
impl ExportSource for Source {
    fn discover(&self, _: BindingId) -> PortFuture<'_, Result<Vec<Target>, PortError>> {
        Box::pin(async move {
            if self.late && self.calls.fetch_add(1, Ordering::Relaxed) > 0 {
                Ok(vec![Target {
                    id: "09".repeat(16),
                    kind: "candidate".into(),
                    identity: "new".into(),
                    digest: None,
                    size: None,
                }])
            } else {
                Ok(vec![])
            }
        })
    }
    fn open<'a>(
        &'a self,
        t: &'a Target,
    ) -> PortFuture<'a, Result<Box<dyn SnapshotSource>, PortError>> {
        Box::pin(async move {
            let p = self.root.join(&t.kind);
            let f = std::fs::File::open(p).map_err(io)?;
            let size = f.metadata().map_err(io)?.len();
            Ok(Box::new(Data {
                file: Mutex::new(f),
                identity: t.identity.clone(),
                size,
            }) as Box<dyn SnapshotSource>)
        })
    }
    fn verify<'a>(&'a self, _: &'a Target) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            if self.mutate || self.changed.load(Ordering::Relaxed) {
                Err(super::super::changed())
            } else {
                Ok(())
            }
        })
    }
}
struct Destination {
    allowed: PathBuf,
    corrupt: bool,
}
struct Directory {
    root: PathBuf,
    files: Mutex<std::collections::HashMap<String, std::fs::File>>,
    corrupt: bool,
}
impl Directory {
    fn reopen(root: PathBuf) -> Self {
        Self {
            root,
            files: Mutex::default(),
            corrupt: false,
        }
    }
}
impl ExportDestination for Destination {
    fn create_new<'a>(
        &'a self,
        d: &'a str,
    ) -> PortFuture<'a, Result<Box<dyn ExportDirectory>, PortError>> {
        Box::pin(async move {
            let p = PathBuf::from(d);
            if p.parent() != Some(self.allowed.as_path())
                || p.file_name().is_none()
                || p.components()
                    .any(|x| matches!(x, std::path::Component::ParentDir))
            {
                return Err(io("目的路径越界"));
            }
            if fs::symlink_metadata(&self.allowed)
                .map_err(io)?
                .file_type()
                .is_symlink()
            {
                return Err(io("不跟随符号链接"));
            }
            fs::create_dir(&p).map_err(io)?;
            Ok(Box::new(Directory {
                root: p,
                files: Mutex::default(),
                corrupt: self.corrupt,
            }) as Box<dyn ExportDirectory>)
        })
    }
}
impl ExportDirectory for Directory {
    fn create<'a>(&'a self, n: &'a str) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(self.root.join(n))
                .map_err(io)?;
            self.files.lock().unwrap().insert(n.into(), f);
            Ok(())
        })
    }
    fn append<'a>(&'a self, n: &'a str, b: &'a [u8]) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let mut files = self.files.lock().unwrap();
            let f = files.get_mut(n).ok_or_else(|| io("未安全打开"))?;
            f.seek(SeekFrom::End(0)).map_err(io)?;
            f.write_all(b).map_err(io)?;
            if self.corrupt && n.ends_with(".bin") {
                f.write_all(b"corrupt").map_err(io)?;
            }
            f.sync_all().map_err(io)
        })
    }
    fn read<'a>(
        &'a self,
        n: &'a str,
        o: u64,
        l: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let mut f = std::fs::File::open(self.root.join(n)).map_err(io)?;
            f.seek(SeekFrom::Start(o)).map_err(io)?;
            let mut b = vec![];
            f.take(l).read_to_end(&mut b).map_err(io)?;
            Ok(b)
        })
    }
    fn finish(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            std::fs::File::open(&self.root)
                .map_err(io)?
                .sync_all()
                .map_err(io)
        })
    }
}
async fn fixture(d: &Dir) -> Arc<Database> {
    let db = setup(d).await;
    let b = B.as_bytes().as_slice();
    sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency,fingerprint,file_identity) VALUES (?,?,7,'awaiting_commit','materialized',x'01',x'02')").bind(b).bind([7u8;16].as_slice()).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,entry_id) VALUES (?,?,'create_conflict_copy',?)").bind([8u8;16].as_slice()).bind(b).bind([7u8;16].as_slice()).execute(db.pool()).await.unwrap();
    let bytes = b"old upload snapshot";
    sqlx::query("INSERT INTO upload_resumes(op_id,binding_id,snapshot_identity,total_size,digest,session_id) VALUES (?,?,'fixed-generation-6',?,?,?)").bind([8u8;16].as_slice()).bind(b).bind(bytes.len() as i64).bind(vaultsyncd_transfer::hash::digest_of(bytes).as_slice()).bind("secret-session-not-exported").execute(db.pool()).await.unwrap();
    fs::write(d.0.join("current"), b"unconfirmed newer bytes").unwrap();
    fs::write(d.0.join("snapshot"), bytes).unwrap();
    db
}
#[tokio::test]
async fn recovery_bundle_keeps_new_dirty_and_fixed_upload_and_validates_output() {
    let d = Dir::new();
    let db = fixture(&d).await;
    let source = Source {
        root: d.0.clone(),
        changed: AtomicBool::new(false),
        mutate: false,
        late: false,
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let sink = Destination {
        allowed: d.0.clone(),
        corrupt: false,
    };
    let p = d.0.join("bundle");
    let m = ExportRecovery::new(db.clone())
        .run(B, p.to_str().unwrap(), &source, &sink)
        .await
        .unwrap();
    assert_eq!(m.files.len(), 3);
    let output = Directory::reopen(p.clone());
    validate(&output, &m).await.unwrap();
    assert_eq!(
        fs::read(p.join(format!("current-{}.bin", "07".repeat(16)))).unwrap(),
        b"unconfirmed newer bytes"
    );
    assert_eq!(
        fs::read(p.join(format!("snapshot-{}.bin", "08".repeat(16)))).unwrap(),
        b"old upload snapshot"
    );
    let state = fs::read_to_string(p.join("state.json")).unwrap();
    assert!(state.contains("create_conflict_copy"));
    assert!(!state.contains("secret-session"));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT dirty FROM local_entries")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "awaiting_commit"
    );
    fs::write(p.join(&m.files[1].name), b"tampered").unwrap();
    assert!(validate(&output, &m).await.is_err());
    assert!(
        ExportRecovery::new(db)
            .run(B, p.to_str().unwrap(), &source, &sink)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn source_change_or_destination_corruption_never_publishes_manifest() {
    for (change, corrupt) in [(true, false), (false, true)] {
        let d = Dir::new();
        let db = fixture(&d).await;
        let source = Source {
            root: d.0.clone(),
            changed: AtomicBool::new(false),
            mutate: change,
            late: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let sink = Destination {
            allowed: d.0.clone(),
            corrupt,
        };
        let p = d.0.join("bundle");
        assert!(
            ExportRecovery::new(db)
                .run(B, p.to_str().unwrap(), &source, &sink)
                .await
                .is_err()
        );
        assert!(!p.join("manifest.json").exists());
        assert_eq!(
            fs::read(d.0.join("current")).unwrap(),
            b"unconfirmed newer bytes"
        );
    }
}
#[cfg(unix)]
#[tokio::test]
async fn traversal_symlink_and_existing_destination_leave_outside_unchanged() {
    let d = Dir::new();
    let db = fixture(&d).await;
    let source = Source {
        root: d.0.clone(),
        changed: AtomicBool::new(false),
        mutate: false,
        late: false,
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let sink = Destination {
        allowed: d.0.clone(),
        corrupt: false,
    };
    let outside = Dir::new();
    fs::write(outside.0.join("precious"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside.0, d.0.join("link")).unwrap();
    for p in [
        d.0.join("link"),
        d.0.join("../outside"),
        d.0.join("link/child"),
    ] {
        assert!(
            ExportRecovery::new(db.clone())
                .run(B, p.to_str().unwrap(), &source, &sink)
                .await
                .is_err()
        );
    }
    assert_eq!(fs::read(outside.0.join("precious")).unwrap(), b"keep");
}
#[tokio::test]
async fn malformed_manifest_is_not_a_verified_recovery_package() {
    let d = Dir::new();
    let directory = Directory::reopen(d.0.clone());
    for binding in ["", &B.to_string()] {
        let m = Manifest {
            format: 1,
            binding: binding.into(),
            files: vec![],
        };
        assert!(validate(&directory, &m).await.is_err());
    }
    for name in ["CON", "../outside", "manifest.json", "bad\0name"] {
        let m = Manifest {
            format: 1,
            binding: B.to_string(),
            files: vec![
                ManifestFile {
                    name: "state.json".into(),
                    size: 0,
                    sha256: "00".repeat(32),
                    source: None,
                },
                ManifestFile {
                    name: name.into(),
                    size: 0,
                    sha256: "00".repeat(32),
                    source: None,
                },
            ],
        };
        assert!(validate(&directory, &m).await.is_err());
    }
}
#[tokio::test]
async fn dirty_metadata_only_is_not_silently_omitted() {
    let d = Dir::new();
    let db = fixture(&d).await;
    sqlx::query("UPDATE local_entries SET residency='metadata_only'")
        .execute(db.pool())
        .await
        .unwrap();
    let (_, targets) = catalog(&db, B).await.unwrap();
    assert!(targets.iter().any(|t| t.kind == "current"));
}

#[tokio::test]
async fn late_candidate_prevents_complete_package() {
    let d = Dir::new();
    let db = fixture(&d).await;
    let source = Source {
        root: d.0.clone(),
        changed: AtomicBool::new(false),
        mutate: false,
        late: true,
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let sink = Destination {
        allowed: d.0.clone(),
        corrupt: false,
    };
    let p = d.0.join("bundle");
    assert!(
        ExportRecovery::new(db)
            .run(B, p.to_str().unwrap(), &source, &sink)
            .await
            .is_err()
    );
    assert!(!p.join("manifest.json").exists());
}
