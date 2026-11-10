//! Extract only the fixed release inventory into a new, empty staging directory.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const FILES: &[&str] = &[
    "vaultsyncd-agent.exe",
    "vaultsyncd-session.exe",
    "vaultsyncd-cli.exe",
    "vaultsyncd-desktop.exe",
    "installed-files.json",
    "UNSIGNED-TEST.txt",
    "Start vaultsyncd.cmd",
    "Stop vaultsyncd.cmd",
    "README-ZIP.txt",
    "zip-files.json",
];
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_TOTAL: u64 = 1024 * 1024 * 1024;

pub(crate) fn extract(archive: &Path, destination: &Path, version: &str) -> io::Result<()> {
    let mut input = File::open(archive)?;
    // zip-rs indexes names in a map, which hides duplicate central-directory entries.
    // Our bounded fixed-file release format never needs multi-disk or ZIP64 counts.
    let length = input.metadata()?.len().min(65557);
    input.seek(SeekFrom::End(-(length as i64)))?;
    let mut tail = vec![0; length as usize];
    input.read_exact(&mut tail)?;
    let footer = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            tail[i..].starts_with(b"PK\x05\x06")
                && i + 22 + u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize == tail.len()
        })
        .ok_or_else(|| io::Error::other("更新包ZIP目录无效"))?;
    let count = FILES.len() as u16;
    if tail[footer + 4..footer + 8] != [0; 4]
        || u16::from_le_bytes([tail[footer + 8], tail[footer + 9]]) != count
        || u16::from_le_bytes([tail[footer + 10], tail[footer + 11]]) != count
    {
        return Err(io::Error::other("更新包ZIP目录数量不符，或含重复条目"));
    }
    let mut zip = zip::ZipArchive::new(input)?;
    if zip.len() != FILES.len() {
        return Err(io::Error::other("更新包文件集合不符"));
    }
    let mut names = BTreeSet::new();
    let mut total = 0;
    // Validate all names before creating output. No archive-provided path is joined.
    for index in 0..zip.len() {
        let item = zip.by_index(index)?;
        if !FILES.contains(&item.name())
            || !names.insert(item.name().to_owned())
            || item.is_dir()
            || item
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            || item.size() > MAX_FILE
        {
            return Err(io::Error::other(
                "更新包含非法路径、链接、重复文件或超大文件",
            ));
        }
        total += item.size();
    }
    if total > MAX_TOTAL {
        return Err(io::Error::other("更新包解压大小超限"));
    }
    fs::create_dir(destination)?;
    for &name in FILES {
        let item = zip.by_name(name)?;
        let size = item.size();
        let mut limited = item.take(size + 1);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(name))?;
        if io::copy(&mut limited, &mut output)? != size {
            return Err(io::Error::other("更新包解压长度不符"));
        }
        output.flush()?;
        output.sync_all()?;
    }
    verify(destination, version)
}

fn metadata(directory: &Path, name: &str) -> io::Result<serde_json::Value> {
    let file = File::open(directory.join(name))?;
    if file.metadata()?.len() > 1024 * 1024 {
        return Err(io::Error::other("更新包清单过大"));
    }
    serde_json::from_reader(file).map_err(io::Error::other)
}
fn verify(directory: &Path, version: &str) -> io::Result<()> {
    let zip = metadata(directory, "zip-files.json")?;
    let installed = metadata(directory, "installed-files.json")?;
    if zip["format"] != 1
        || installed["format"] != 1
        || zip["signature_status"] != "unsigned-test"
        || installed["signature_status"] != "unsigned-test"
        || zip["source"] != installed["source"]
        || installed["source"]["version"].as_str() != Some(version)
    {
        return Err(io::Error::other("更新包版本或同源清单不符"));
    }
    for (manifest, names) in [(&zip, &FILES[..FILES.len() - 1]), (&installed, &FILES[..4])] {
        let entries = manifest["files"]
            .as_array()
            .ok_or_else(|| io::Error::other("更新包缺少文件清单"))?;
        if entries.len() != names.len() {
            return Err(io::Error::other("更新包清单数量不符"));
        }
        for &name in names {
            let matches: Vec<_> = entries
                .iter()
                .filter(|item| item["path"].as_str() == Some(name))
                .collect();
            if matches.len() != 1 {
                return Err(io::Error::other("更新包清单缺失或重复"));
            }
            let mut input = File::open(directory.join(name))?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            if matches[0]["sha256"].as_str() != Some(&format!("{:x}", hash.finalize()))
                || matches[0]["bytes"].as_u64() != Some(input.metadata()?.len())
            {
                return Err(io::Error::other(format!("更新包文件校验失败：{name}")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
