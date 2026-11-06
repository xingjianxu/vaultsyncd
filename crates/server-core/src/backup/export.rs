//! 清单授权与增量归档验证。仅消费字节，不打开文件、socket或数据库。
use crate::objects::{ContentDigest, ObjectLayout};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use vaultsyncd_domain::ids::{BackupLeaseId, RootId, ServerId};

/// 固定流格式：magic、u64清单长度、清单、各项(path长度/path/u64字节数/字节)、u32零终止。
pub const MAGIC: &[u8; 8] = b"VSBK001\n";
/// 清单解析上限；对象字节不受此上限限制，按64KiB流动。
pub const MAX_MANIFEST: usize = 256 * 1024 * 1024;
/// 每个适配器的流块上限。
pub const CHUNK: usize = 64 * 1024;
/// 归档未完整或不符合固定清单，绝不能发布verified。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 清单/路径/身份/格式非法。
    Invalid,
    /// 清单自身与受信摘要不同。
    ManifestMismatch,
    /// 字节数或内容摘要不同。
    ContentMismatch,
    /// 缺头、缺对象或缺最终终止标记。
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
/// 内部授权项，不是第二份wire DTO；外部清单格式唯一源为backup-manifest.schema.json。
#[derive(Clone, Debug)]
pub struct Item {
    /// 仅清单允许的归档相对路径。
    pub path: String,
    /// SHA-256。
    pub hash: [u8; 32],
    /// 字节数。
    pub size: u64,
}
/// 从已校验清单提取的内部授权索引。
#[derive(Clone, Debug)]
pub struct Index {
    /// 备份身份。
    pub id: BackupLeaseId,
    /// 服务器身份。
    pub server: ServerId,
    /// 固定清单摘要。
    pub manifest_hash: [u8; 32],
    /// 排序后的全部文件，不含清单自身。
    pub items: Vec<Item>,
    /// 固定Root序号，用于导出数据库语义核对。
    pub root_seqs: std::collections::BTreeMap<RootId, u64>,
    /// 数据库版本，与产品/协议版本独立。
    pub schema_version: i64,
    /// 清单声明的逻辑表计数。
    pub table_counts: std::collections::BTreeMap<String, u64>,
}
fn field<'a>(v: &'a Value, k: &str) -> Result<&'a str, Error> {
    v[k].as_str().ok_or(Error::Invalid)
}
fn digest_text(v: &Value, k: &str) -> Result<[u8; 32], Error> {
    ContentDigest::from_hex(field(v, k)?)
        .map(|d| *d.as_bytes())
        .map_err(|_| Error::Invalid)
}
fn decimal(v: &Value, k: &str) -> Result<u64, Error> {
    let s = field(v, k)?;
    let n = s.parse::<u64>().map_err(|_| Error::Invalid)?;
    if n.to_string() != s || n > i64::MAX as u64 {
        return Err(Error::Invalid);
    }
    Ok(n)
}
impl Index {
    /// 校验固定清单、规范Root/对象路径、唯一文件位置及必须具备的DB/配置。
    pub fn parse(bytes: &[u8], expected: [u8; 32]) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > MAX_MANIFEST {
            return Err(Error::Invalid);
        }
        let actual: [u8; 32] = Sha256::digest(bytes).into();
        if actual != expected {
            return Err(Error::ManifestMismatch);
        }
        let v: Value = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        if v["format_version"] != 1
            || v["state"] != "prepared"
            || v["scope"] != "confirmed_logical_state"
            || v["sessions"] != "invalidated"
            || v["external_uncollected_writes"] != "excluded"
        {
            return Err(Error::Invalid);
        }
        let id = BackupLeaseId::from_hex(field(&v, "backup_id")?).map_err(|_| Error::Invalid)?;
        let server = ServerId::from_hex(field(&v, "server_id")?).map_err(|_| Error::Invalid)?;
        let mut roots = BTreeSet::new();
        let mut root_seqs = std::collections::BTreeMap::new();
        for r in v["roots"].as_array().ok_or(Error::Invalid)? {
            let root = RootId::from_hex(field(r, "root_id")?).map_err(|_| Error::Invalid)?;
            let seq = decimal(r, "seq")?;
            if !roots.insert(root) {
                return Err(Error::Invalid);
            }
            root_seqs.insert(root, seq);
        }
        let schema_version = v["schema_version"]
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or(Error::Invalid)?;
        let mut table_counts = std::collections::BTreeMap::new();
        for table in [
            "users",
            "roots",
            "entries",
            "versions",
            "trash_entries",
            "tombstones",
            "changes",
            "operations",
        ] {
            table_counts.insert(table.to_owned(), decimal(&v["database_contents"], table)?);
        }
        let mut items = Vec::new();
        let mut names = BTreeSet::new();
        for f in v["files"].as_array().ok_or(Error::Invalid)? {
            let path = field(f, "path")?.to_owned();
            if !["database.sqlite", "config.json"].contains(&path.as_str())
                || !names.insert(path.clone())
            {
                return Err(Error::Invalid);
            }
            items.push(Item {
                path,
                hash: digest_text(f, "sha256")?,
                size: decimal(f, "size")?,
            });
        }
        if names.len() != 2 {
            return Err(Error::Invalid);
        }
        for o in v["objects"].as_array().ok_or(Error::Invalid)? {
            let root = RootId::from_hex(field(o, "root_id")?).map_err(|_| Error::Invalid)?;
            let hash = digest_text(o, "sha256")?;
            let path = format!(
                "objects/{}/{}",
                root.to_hex(),
                ContentDigest::from_bytes(hash).to_hex()
            );
            if !roots.contains(&root)
                || field(o, "archive_path")? != path
                || field(o, "storage_path")?
                    != ObjectLayout::relative_object_path(&ContentDigest::from_bytes(hash))
                        .to_str()
                        .ok_or(Error::Invalid)?
                || !names.insert(path.clone())
            {
                return Err(Error::Invalid);
            }
            items.push(Item {
                path,
                hash,
                size: decimal(o, "size")?,
            });
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            id,
            server,
            manifest_hash: actual,
            items,
            root_seqs,
            schema_version,
            table_counts,
        })
    }
    /// 只按清单精确查找，不能将哈希变成任意服务器路径。
    pub fn item(&self, path: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.path == path)
    }
}
/// 一项的确定性流头。
pub fn item_header(item: &Item) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&(item.path.len() as u32).to_be_bytes());
    h.extend_from_slice(item.path.as_bytes());
    h.extend_from_slice(&item.size.to_be_bytes());
    h
}
/// 流开始的固定16字节；清单随后单独按块发送。
pub fn prefix(manifest_len: usize) -> Result<Vec<u8>, Error> {
    if manifest_len == 0 || manifest_len > MAX_MANIFEST {
        return Err(Error::Invalid);
    }
    let mut h = MAGIC.to_vec();
    h.extend_from_slice(&(manifest_len as u64).to_be_bytes());
    Ok(h)
}
/// 有界增量内容检查。
pub struct Integrity {
    hash: Sha256,
    size: u64,
    expected: Item,
}
impl Integrity {
    /// 建立一个清单项的校验器。
    pub fn new(item: Item) -> Self {
        Self {
            hash: Sha256::new(),
            size: 0,
            expected: item,
        }
    }
    /// 输入实际流字节；过长立即拒绝。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.size = self
            .size
            .checked_add(bytes.len() as u64)
            .ok_or(Error::ContentMismatch)?;
        if self.size > self.expected.size {
            return Err(Error::ContentMismatch);
        }
        self.hash.update(bytes);
        Ok(())
    }
    /// 缺字节与错摘要都不能当成功。
    pub fn finish(self) -> Result<(), Error> {
        if self.size != self.expected.size
            || <[u8; 32]>::from(self.hash.finalize()) != self.expected.hash
        {
            return Err(Error::ContentMismatch);
        }
        Ok(())
    }
}
enum Phase {
    Start(Vec<u8>),
    Manifest { remaining: usize, bytes: Vec<u8> },
    Header { expected: Vec<u8>, offset: usize },
    Data { remaining: u64, check: Integrity },
    Done,
}
/// 任意分块的归档验证状态机；不缓冲对象，仅清单有明确上限。
pub struct ArchiveVerifier {
    phase: Phase,
    index: Option<Index>,
    next: usize,
    expected_manifest: [u8; 32],
    failed: bool,
}
impl ArchiveVerifier {
    /// 摘要来自已认证服务状态或之前固定的导出收据。
    pub fn new(expected_manifest: [u8; 32]) -> Self {
        Self {
            phase: Phase::Start(Vec::new()),
            index: None,
            next: 0,
            expected_manifest,
            failed: false,
        }
    }
    fn next_header(&self) -> Phase {
        let expected = if let Some(i) = self.index.as_ref().and_then(|i| i.items.get(self.next)) {
            item_header(i)
        } else {
            vec![0; 4]
        };
        Phase::Header {
            expected,
            offset: 0,
        }
    }
    /// 输入传输或磁盘读出的实际字节。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Incomplete);
        }
        let result = self.push_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn push_inner(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        loop {
            match &mut self.phase {
                Phase::Start(h) => {
                    let n = (16 - h.len()).min(bytes.len());
                    h.extend_from_slice(&bytes[..n]);
                    bytes = &bytes[n..];
                    if h.len() != 16 {
                        return Ok(());
                    }
                    if &h[..8] != MAGIC {
                        return Err(Error::Invalid);
                    }
                    let len = u64::from_be_bytes(h[8..].try_into().map_err(|_| Error::Invalid)?);
                    if len == 0 || len > MAX_MANIFEST as u64 {
                        return Err(Error::Invalid);
                    }
                    self.phase = Phase::Manifest {
                        remaining: len as usize,
                        bytes: Vec::with_capacity(len as usize),
                    };
                }
                Phase::Manifest {
                    remaining,
                    bytes: manifest,
                } => {
                    let n = (*remaining).min(bytes.len());
                    manifest.extend_from_slice(&bytes[..n]);
                    *remaining -= n;
                    bytes = &bytes[n..];
                    if *remaining != 0 {
                        return Ok(());
                    }
                    self.index = Some(Index::parse(manifest, self.expected_manifest)?);
                    self.phase = self.next_header();
                }
                Phase::Header { expected, offset } => {
                    let n = (expected.len() - *offset).min(bytes.len());
                    if expected[*offset..*offset + n] != bytes[..n] {
                        return Err(Error::Invalid);
                    }
                    *offset += n;
                    bytes = &bytes[n..];
                    if *offset < expected.len() {
                        return Ok(());
                    }
                    if let Some(item) = self
                        .index
                        .as_ref()
                        .and_then(|i| i.items.get(self.next))
                        .cloned()
                    {
                        self.phase = Phase::Data {
                            remaining: item.size,
                            check: Integrity::new(item),
                        };
                    } else {
                        self.phase = Phase::Done;
                    }
                }
                Phase::Data { remaining, check } => {
                    let n = (*remaining).min(bytes.len() as u64) as usize;
                    check.push(&bytes[..n])?;
                    *remaining -= n as u64;
                    bytes = &bytes[n..];
                    if *remaining != 0 {
                        return Ok(());
                    }
                    let old = std::mem::replace(&mut self.phase, Phase::Done);
                    if let Phase::Data { check, .. } = old {
                        check.finish()?;
                    }
                    self.next += 1;
                    self.phase = self.next_header();
                }
                Phase::Done => {
                    if !bytes.is_empty() {
                        return Err(Error::Invalid);
                    }
                    return Ok(());
                }
            }
            if bytes.is_empty() && !matches!(&self.phase, Phase::Data { remaining: 0, .. }) {
                return Ok(());
            }
        }
    }
    /// 必须见到终止标记，返回固定授权索引而非默认空成功。
    pub fn finish(self) -> Result<Index, Error> {
        if self.failed || !matches!(self.phase, Phase::Done) {
            return Err(Error::Incomplete);
        }
        self.index.ok_or(Error::Incomplete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Vec<u8>, [u8; 32], Vec<u8>) {
        let db = b"database";
        let config = b"config";
        let value = serde_json::json!({"format_version":1,"state":"prepared","backup_id":"01010101010101010101010101010101","server_id":"02020202020202020202020202020202","schema_version":13,"scope":"confirmed_logical_state","sessions":"invalidated","external_uncollected_writes":"excluded","roots":[],"objects":[],"database_contents":{"users":"1","roots":"0","entries":"0","versions":"0","trash_entries":"0","tombstones":"0","changes":"0","operations":"0"},"files":[{"path":"database.sqlite","size":db.len().to_string(),"sha256":ContentDigest::from_bytes(Sha256::digest(db).into()).to_hex()},{"path":"config.json","size":config.len().to_string(),"sha256":ContentDigest::from_bytes(Sha256::digest(config).into()).to_hex()}]});
        let manifest = serde_json::to_vec(&value).unwrap();
        let hash = Sha256::digest(&manifest).into();
        let index = Index::parse(&manifest, hash).unwrap();
        let mut bytes = prefix(manifest.len()).unwrap();
        bytes.extend_from_slice(&manifest);
        for i in &index.items {
            bytes.extend(item_header(i));
            bytes.extend_from_slice(if i.path == "config.json" {
                config.as_slice()
            } else {
                db.as_slice()
            });
        }
        bytes.extend([0; 4]);
        (manifest, hash, bytes)
    }
    #[test]
    fn arbitrary_chunks_validate_and_missing_trailer_is_incomplete() {
        let (_, hash, bytes) = fixture();
        let mut v = ArchiveVerifier::new(hash);
        for byte in &bytes {
            v.push(&[*byte]).unwrap();
        }
        assert_eq!(v.finish().unwrap().items.len(), 2);
        let mut v = ArchiveVerifier::new(hash);
        v.push(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(v.finish().unwrap_err(), Error::Incomplete);
    }
    #[test]
    fn corrupt_bytes_and_unlisted_paths_never_verify() {
        let (_, hash, mut bytes) = fixture();
        let n = bytes.len();
        bytes[n - 5] ^= 1;
        let mut v = ArchiveVerifier::new(hash);
        assert_eq!(v.push(&bytes), Err(Error::ContentMismatch));
        assert_eq!(v.finish().unwrap_err(), Error::Incomplete);
        let (_, hash, mut trailing) = fixture();
        trailing.push(1);
        let mut v = ArchiveVerifier::new(hash);
        assert_eq!(v.push(&trailing), Err(Error::Invalid));
        assert_eq!(v.finish().unwrap_err(), Error::Incomplete);
        let (manifest, hash, _) = fixture();
        let mut value: Value = serde_json::from_slice(&manifest).unwrap();
        value["files"][0]["path"] = "../../user-data".into();
        let bad = serde_json::to_vec(&value).unwrap();
        assert!(Index::parse(&bad, Sha256::digest(&bad).into()).is_err());
        assert_eq!(
            Index::parse(&bad, hash).unwrap_err(),
            Error::ManifestMismatch
        );
    }
}
