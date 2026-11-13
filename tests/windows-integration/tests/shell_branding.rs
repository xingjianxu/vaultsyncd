//! S222: isolated Explorer presentation fixture, driven by s222-shell-branding.ps1.
#![cfg(windows)]

use std::{path::PathBuf, sync::Arc};
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::Catalog,
    },
    shell,
};

#[test]
fn native_shell_branding_fixture() {
    let Some(base) = std::env::var_os("VAULTSYNCD_S222_ROOT") else {
        return;
    };
    let base = PathBuf::from(base);
    let parent = PathBuf::from(r"C:\vaultsyncd-lab");
    assert_eq!(base.parent(), Some(parent.as_path()));
    assert!(
        base.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("s222-")
    );
    let bindings = std::env::var("VAULTSYNCD_S222_BINDINGS").unwrap();
    let bindings: Vec<_> = bindings.split(',').collect();
    assert_eq!(bindings.len(), 3);
    let action = std::env::var("VAULTSYNCD_S222_ACTION").unwrap();
    assert!(matches!(action.as_str(), "register" | "cleanup"));
    for (name, binding) in ["资料, 项目 @alpha", "工作文档", r"other\资料, 项目 @alpha"]
        .into_iter()
        .zip(bindings)
    {
        let path = base.join(name);
        let binding = binding.parse().unwrap();
        let catalog = Catalog::new(binding, "22222222222222222222222222222222".parse().unwrap());
        if action == "register" {
            std::fs::create_dir_all(&path).unwrap();
            if !path.join("keep.txt").exists() {
                std::fs::write(path.join("keep.txt"), b"S222 unconfirmed local content").unwrap();
            }
            shell::register(&path, &catalog).unwrap();
            assert!(shell::registered(binding).unwrap());
        } else {
            let mut adapter = Adapter::connect(&path, catalog, Arc::new(MetadataOnly)).unwrap();
            adapter.unregister().unwrap();
            shell::unregister(binding).unwrap();
            assert!(!shell::registered(binding).unwrap());
        }
        assert_eq!(
            std::fs::read(path.join("keep.txt")).unwrap(),
            b"S222 unconfirmed local content"
        );
        println!(
            "check=pass action={action} path={} binding={binding}",
            path.display()
        );
    }
}
