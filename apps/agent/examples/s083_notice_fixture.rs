//! S083 controlled offline fixture: canonical core issue states, not a real full/missing volume.
#[cfg(not(windows))]
fn main() {
    eprintln!("S083 fixture runs only on the Windows laboratory guest");
    std::process::exit(2);
}
#[cfg(windows)]
#[tokio::main]
async fn main() {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt, path::PathBuf};
    use vaultsyncd_domain::{error::ErrorCode, ids::RootId};
    let path = PathBuf::from(std::env::args_os().nth(1).expect("state path"));
    let action = std::env::args().nth(2).expect("action");
    let path = path.canonicalize().expect("existing state directory");
    let base = PathBuf::from("C:/vaultsyncd-lab").canonicalize().unwrap();
    assert!(
        path.starts_with(base)
            && path
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with("s083-"))
            && path.file_name().unwrap() == "state"
    );
    let file = path.join("client.sqlite");
    if file.exists() {
        drop(
            OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(&file)
                .expect("refuse to modify a live database"),
        );
    }
    let db = vaultsyncd_client_store::db::Database::open(file)
        .await
        .unwrap();
    let root = RootId::from_bytes([0x83; 16]);
    use vaultsyncd_agent_core::status::issues;
    match action.as_str() {
        "seed" => {
            issues::report(&db, root, ErrorCode::InsufficientStorage, false)
                .await
                .unwrap();
            issues::report(&db, root, ErrorCode::MountFailed, false)
                .await
                .unwrap();
            issues::report_conflict(&db, root).await.unwrap();
        }
        "duplicate" => {
            for _ in 0..20 {
                issues::report(&db, root, ErrorCode::InsufficientStorage, false)
                    .await
                    .unwrap();
            }
        }
        "resolve" => {
            issues::resolve_root(&db, root, ErrorCode::InsufficientStorage)
                .await
                .unwrap();
            issues::resolve_root(&db, root, ErrorCode::MountFailed)
                .await
                .unwrap();
        }
        "recur" => {
            issues::report(&db, root, ErrorCode::InsufficientStorage, false)
                .await
                .unwrap();
        }
        "offline" => {
            for _ in 0..100 {
                assert!(
                    !issues::report(&db, root, ErrorCode::RateLimited, false)
                        .await
                        .unwrap()
                        .notify
                );
            }
        }
        _ => panic!("unknown offline fixture action"),
    }
    db.close().await;
    println!("core_fixture={action} offline=true volume_fault=false");
}
