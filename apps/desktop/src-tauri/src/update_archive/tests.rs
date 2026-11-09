use super::*;
use std::{collections::BTreeMap, io::Write};
struct Fixture(PathBuf);
use std::path::PathBuf;
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "S158-archive-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    Fixture(root)
}
fn files() -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<_, _> = FILES[..10]
        .iter()
        .map(|&name| (name.to_owned(), name.as_bytes().to_vec()))
        .collect();
    let entry = |name: &str, data: &[u8]| serde_json::json!({"path":name,"bytes":data.len(),"sha256":format!("{:x}",Sha256::digest(data))});
    let source = serde_json::json!({"version":"0.2.0","commit":"fixture"});
    let installed = serde_json::json!({"format":1,"signature_status":"unsigned-test","source":source,"files":FILES[..4].iter().map(|name|entry(name,&files[*name])).collect::<Vec<_>>()});
    files.insert(
        "installed-files.json".into(),
        serde_json::to_vec(&installed).unwrap(),
    );
    let zip = serde_json::json!({"format":1,"signature_status":"unsigned-test","source":source,"files":files.iter().map(|(name,data)|entry(name,data)).collect::<Vec<_>>()});
    files.insert("zip-files.json".into(), serde_json::to_vec(&zip).unwrap());
    files
}
fn archive(path: &Path, files: &BTreeMap<String, Vec<u8>>) {
    let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, data) in files {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}
#[test]
fn exact_inventory_extracts_without_overwriting_and_checks_version() {
    let f = fixture();
    let path = f.0.join("update.zip");
    archive(&path, &files());
    extract(&path, &f.0.join("中文 程序"), "0.2.0").unwrap();
    assert!(extract(&path, &f.0.join("中文 程序"), "0.2.0").is_err());
    assert!(extract(&path, &f.0.join("wrong-version"), "0.3.0").is_err());
}
#[test]
fn traversal_extra_missing_and_corruption_are_rejected() {
    let f = fixture();
    for bad in [
        "../escape",
        "nested/program.exe",
        "C:\\escape.exe",
        "vaultsyncd-agent.exe:ads",
    ] {
        let mut files = files();
        let bytes = files.remove("README-ZIP.txt").unwrap();
        files.insert(bad.into(), bytes);
        let path = f.0.join("bad.zip");
        archive(&path, &files);
        assert!(extract(&path, &f.0.join("output"), "0.2.0").is_err());
        assert!(!f.0.join("output").exists());
    }
    for kind in ["extra", "missing", "corrupt", "mixed"] {
        let mut files = files();
        match kind {
            "extra" => {
                files.insert("unknown.exe".into(), vec![]);
            }
            "missing" => {
                files.remove("README-ZIP.txt");
            }
            "corrupt" => {
                files.insert("vaultsyncd-agent.exe".into(), b"tampered".to_vec());
            }
            _ => {
                let mut m: serde_json::Value =
                    serde_json::from_slice(&files["installed-files.json"]).unwrap();
                m["source"]["commit"] = "different".into();
                files.insert(
                    "installed-files.json".into(),
                    serde_json::to_vec(&m).unwrap(),
                );
            }
        }
        let path = f.0.join(format!("{kind}.zip"));
        archive(&path, &files);
        assert!(extract(&path, &f.0.join(kind), "0.2.0").is_err());
    }
}

#[test]
fn duplicate_central_directory_entries_cannot_be_hidden_by_the_zip_index() {
    let f = fixture();
    let path = f.0.join("duplicate.zip");
    archive(&path, &files());
    let mut bytes = fs::read(&path).unwrap();
    let first = bytes.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
    let field = |offset: usize| {
        u16::from_le_bytes([bytes[first + offset], bytes[first + offset + 1]]) as usize
    };
    let size = 46 + field(28) + field(30) + field(32);
    let repeated = bytes[first..first + size].to_vec();
    let end = bytes.len() - 22;
    let old_size = u32::from_le_bytes(bytes[end + 12..end + 16].try_into().unwrap());
    bytes.splice(end..end, repeated);
    let end = end + size;
    for offset in [8, 10] {
        bytes[end + offset..end + offset + 2].copy_from_slice(&12u16.to_le_bytes());
    }
    bytes[end + 12..end + 16].copy_from_slice(&(old_size + size as u32).to_le_bytes());
    fs::write(&path, bytes).unwrap();
    assert!(extract(&path, &f.0.join("output"), "0.2.0").is_err());
    assert!(!f.0.join("output").exists());
}
