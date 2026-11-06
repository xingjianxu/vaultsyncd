//! 只在离线独占锁下恢复到新空目录；候选、普通文件和最终成功均重新核验。
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use vaultsyncd_domain::ids::RootId;
use vaultsyncd_platform_linux_server::{
    files::{ResolveMode, RestrictedDir, hash_file, root::RootSnapshot},
    mounts::admission,
    restore::Target,
};
use vaultsyncd_server_core::{
    backup::{
        export::{self, ArchiveVerifier, CHUNK, Index, Item, MAX_MANIFEST},
        restore::{self, Compatibility, Mapping},
    },
    objects::{ContentDigest, ObjectLayout},
};
use vaultsyncd_server_store::{
    restore::{Plan, Remap},
    state_lock::{self, RESTORE_INCOMPLETE, StateLock, StateLockError},
};

pub const USAGE: &str = "restore validate|apply --archive ABSOLUTE.vsbk --state-dir EMPTY_ABSOLUTE --root ROOT_ID=EMPTY_ABSOLUTE [--root ...] [--json]\n先validate，停服务后apply；全量Root必须映射到新空CoW btrfs目录。失败现场保留并阻止服务启动；重试用新空目标，不覆盖半成品。归档及固定摘要收据须一起保管。";
struct Args {
    apply: bool,
    archive: PathBuf,
    state: PathBuf,
    roots: Vec<Mapping>,
    json: bool,
}
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn bad(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s)
}
fn parse(args: Vec<OsString>) -> io::Result<Args> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("restore")) {
        return Err(bad("expected restore"));
    }
    let apply = match args.next().as_deref() {
        Some(s) if s == "apply" => true,
        Some(s) if s == "validate" => false,
        _ => return Err(bad("expected validate or apply")),
    };
    let mut archive = None;
    let mut state = None;
    let mut roots = Vec::new();
    let mut json = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--archive") if archive.is_none() => {
                archive = Some(PathBuf::from(
                    args.next().ok_or_else(|| bad("archive missing"))?,
                ))
            }
            Some("--state-dir") if state.is_none() => {
                state = Some(PathBuf::from(
                    args.next().ok_or_else(|| bad("state directory missing"))?,
                ))
            }
            Some("--root") => {
                let text = args
                    .next()
                    .and_then(|s| s.into_string().ok())
                    .ok_or_else(|| bad("Root mapping missing"))?;
                let (id, path) = text
                    .split_once('=')
                    .ok_or_else(|| bad("Root mapping needs ID=absolute-path"))?;
                let root = RootId::from_hex(id).map_err(err)?;
                if root.to_hex() != id {
                    return Err(bad("noncanonical Root ID"));
                }
                roots.push(Mapping {
                    root,
                    path: path.into(),
                });
            }
            Some("--json") if !json => json = true,
            _ => return Err(bad("unknown or duplicate restore argument")),
        }
    }
    let archive = archive.ok_or_else(|| bad("archive required"))?;
    let state = state.ok_or_else(|| bad("state directory required"))?;
    if !restore::canonical_path(&archive)
        || !restore::canonical_path(&state)
        || roots.iter().any(|m| !restore::canonical_path(&m.path))
    {
        return Err(bad("restore paths must be canonical absolute paths"));
    }
    Ok(Args {
        apply,
        archive,
        state,
        roots,
        json,
    })
}
fn random_hex() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    vaultsyncd_domain::ids::BackupLeaseId::from_bytes(b).to_hex()
}
fn read_limited(mut file: File, limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    (&mut file).take((limit + 1) as u64).read_to_end(&mut out)?;
    if out.len() > limit {
        return Err(bad("backup document too large"));
    }
    Ok(out)
}
fn open_input(path: &Path) -> io::Result<File> {
    let parent = path.parent().ok_or_else(|| bad("input parent missing"))?;
    if std::fs::canonicalize(parent)?.as_os_str() != parent.as_os_str() {
        return Err(bad("backup parent path must be canonical"));
    }
    let dir = RestrictedDir::open(parent, ResolveMode::Auto).map_err(err)?;
    let file = dir
        .open_read(Path::new(
            path.file_name().ok_or_else(|| bad("input name missing"))?,
        ))
        .map_err(err)?
        .file;
    if !file.metadata()?.is_file() {
        return Err(bad("backup input must be a regular file"));
    }
    Ok(file)
}
struct Source {
    file: File,
    index: Index,
    manifest: Vec<u8>,
    value: Value,
}
impl Source {
    fn open(path: &Path) -> io::Result<Self> {
        let mut receipt_name = path.as_os_str().to_os_string();
        receipt_name.push(".verification.json");
        let receipt: Value = serde_json::from_slice(&read_limited(
            open_input(Path::new(&receipt_name))?,
            1024 * 1024,
        )?)
        .map_err(err)?;
        if receipt["format_version"] != 1 {
            return Err(bad("unsupported backup receipt"));
        }
        let hash = ContentDigest::from_hex(
            receipt["manifest_hash"]
                .as_str()
                .ok_or_else(|| bad("pinned manifest digest missing"))?,
        )
        .map_err(err)?;
        let mut file = open_input(path)?;
        let mut head = [0; 16];
        file.read_exact(&mut head)?;
        if &head[..8] != export::MAGIC {
            return Err(bad("unsupported backup archive"));
        }
        let n = u64::from_be_bytes(head[8..].try_into().map_err(err)?);
        if n == 0 || n > MAX_MANIFEST as u64 {
            return Err(bad("invalid manifest length"));
        }
        let mut manifest = vec![0; n as usize];
        file.read_exact(&mut manifest)?;
        let index = Index::parse(&manifest, *hash.as_bytes()).map_err(err)?;
        if receipt["backup_id"] != index.id.to_hex() {
            return Err(bad("backup receipt identity mismatch"));
        }
        let value = serde_json::from_slice(&manifest).map_err(err)?;
        Ok(Self {
            file,
            index,
            manifest,
            value,
        })
    }
    /// 所有项均由唯一增量验证器检查；sink只决定是否保留该项，不绕过其字节验证。
    fn stream(
        &mut self,
        mut sink: impl FnMut(&Item) -> io::Result<Option<File>>,
    ) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut verifier = ArchiveVerifier::new(self.index.manifest_hash);
        let prefix = export::prefix(self.manifest.len()).map_err(err)?;
        let mut actual = vec![0; prefix.len()];
        self.file.read_exact(&mut actual)?;
        if actual != prefix {
            return Err(err("archive prefix changed"));
        }
        verifier.push(&actual).map_err(err)?;
        let mut manifest = vec![0; self.manifest.len()];
        self.file.read_exact(&mut manifest)?;
        if manifest != self.manifest {
            return Err(err("manifest changed"));
        }
        verifier.push(&manifest).map_err(err)?;
        let mut bytes = vec![0; CHUNK];
        for item in &self.index.items {
            let expected = export::item_header(item);
            let mut header = vec![0; expected.len()];
            self.file.read_exact(&mut header)?;
            if header != expected {
                return Err(err("archive item differs from fixed manifest"));
            }
            verifier.push(&header).map_err(err)?;
            let mut output = sink(item)?;
            let mut left = item.size;
            while left != 0 {
                let n = usize::try_from(left.min(CHUNK as u64)).map_err(err)?;
                self.file.read_exact(&mut bytes[..n])?;
                verifier.push(&bytes[..n]).map_err(err)?;
                if let Some(out) = &mut output {
                    out.write_all(&bytes[..n])?;
                }
                left -= n as u64;
            }
            if let Some(out) = output {
                out.sync_all()?;
            }
        }
        let mut terminal = [0; 4];
        self.file.read_exact(&mut terminal)?;
        verifier.push(&terminal).map_err(err)?;
        let mut extra = [0; 1];
        if self.file.read(&mut extra)? != 0 {
            verifier.push(&extra).map_err(err)?;
            return Err(err("archive has trailing bytes"));
        }
        verifier.finish().map_err(err)?;
        Ok(())
    }
}
struct Scratch<'a> {
    parent: &'a RestrictedDir,
    dir: RestrictedDir,
    name: String,
    cleaned: bool,
}
impl<'a> Scratch<'a> {
    fn new(parent: &'a RestrictedDir) -> io::Result<Self> {
        let name = format!(".restore-validation-{}", random_hex());
        parent.mkdir(Path::new(&name), 0o700).map_err(err)?;
        let dir = parent.child_dir(Path::new(&name)).map_err(err)?;
        Ok(Self {
            parent,
            dir,
            name,
            cleaned: false,
        })
    }
    fn clean(&mut self) -> io::Result<()> {
        for name in ["database.sqlite", "config.json"] {
            if self
                .dir
                .identity_at(Path::new(name))
                .map_err(err)?
                .is_some()
            {
                self.dir.unlink(Path::new(name)).map_err(err)?;
            }
        }
        self.parent.unlink_dir(Path::new(&self.name)).map_err(err)?;
        self.parent.fsync_dir().map_err(err)?;
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.clean();
        }
    }
}
fn mkdir_parents(dir: &RestrictedDir, path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    if let Some(parent) = path.parent() {
        for part in parent.components() {
            current.push(part);
            if dir.identity_at(&current).map_err(err)?.is_none() {
                dir.mkdir(&current, 0o700).map_err(err)?;
            }
            dir.child_dir(&current).map_err(err)?;
        }
    }
    Ok(())
}
fn create(dir: &RestrictedDir, path: &Path, mode: u32) -> io::Result<File> {
    mkdir_parents(dir, path)?;
    dir.create_exclusive(path, mode).map_err(err)
}
fn write_document(dir: &RestrictedDir, name: &str, bytes: &[u8]) -> io::Result<()> {
    let mut file = create(dir, Path::new(name), 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    dir.fsync_dir().map_err(err)
}
fn check_file(dir: &RestrictedDir, path: &Path, hash: [u8; 32], size: u64) -> io::Result<()> {
    let opened = dir.open_read(path).map_err(err)?;
    if opened.identity.nlink != 1 || hash_file(&opened.file)? != (hash, size) {
        return Err(err("restored ordinary file identity/hash mismatch"));
    }
    Ok(())
}
fn verify_work(target: &Target, work: &[restore::Work]) -> io::Result<()> {
    let expected: BTreeMap<_, _> = work.iter().map(|w| (w.path.clone(), w.content)).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(parent) = stack.pop() {
        let child = if parent.as_os_str().is_empty() {
            None
        } else {
            Some(target.dir.child_dir(&parent).map_err(err)?)
        };
        let dir = child.as_ref().unwrap_or(&target.dir);
        for e in dir.list_entries().map_err(err)? {
            let path = parent.join(e.name);
            let content = expected
                .get(&path)
                .ok_or_else(|| err("unexpected file in restore target; preserved"))?;
            if !seen.insert(path.clone()) {
                return Err(err("duplicate restored path"));
            }
            match content {
                None => {
                    target.dir.child_dir(&path).map_err(err)?;
                    stack.push(path)
                }
                Some((hash, size)) => check_file(&target.dir, &path, *hash, *size)?,
            }
        }
    }
    if seen.len() != expected.len() {
        return Err(err("restored work tree is incomplete"));
    }
    target.verify_identity()
}
fn sync_tree(dir: &RestrictedDir) -> io::Result<()> {
    let mut stack = vec![PathBuf::new()];
    let mut directories = Vec::new();
    while let Some(path) = stack.pop() {
        let child = if path.as_os_str().is_empty() {
            None
        } else {
            Some(dir.child_dir(&path).map_err(err)?)
        };
        let current = child.as_ref().unwrap_or(dir);
        for entry in current.list_entries().map_err(err)? {
            let relative = path.join(entry.name);
            let identity = dir.open_path(&relative).map_err(err)?.identity;
            if identity.mode & 0o170000 == 0o040000 {
                stack.push(relative);
            }
        }
        directories.push(path);
    }
    for path in directories.into_iter().rev() {
        if path.as_os_str().is_empty() {
            dir.fsync_dir().map_err(err)?;
        } else {
            dir.child_dir(&path)
                .map_err(err)?
                .fsync_dir()
                .map_err(err)?;
        }
    }
    Ok(())
}
fn fresh_epoch(plan: &Plan) -> u64 {
    use rand_core::RngCore;
    loop {
        let epoch = rand_core::OsRng.next_u64() & i64::MAX as u64;
        if epoch != 0 && !plan.epochs.values().any(|old| *old == epoch) {
            return epoch;
        }
    }
}
fn config(source: &Value, state: &Path, roots: &BTreeMap<RootId, PathBuf>) -> io::Result<String> {
    let listen = source["server"]["listen"]
        .as_str()
        .ok_or_else(|| bad("backup listen missing"))?;
    listen.parse::<std::net::SocketAddr>().map_err(err)?;
    let level = source["log"]["level"]
        .as_str()
        .ok_or_else(|| bad("backup log level missing"))?
        .to_ascii_lowercase();
    let format = source["log"]["format"]
        .as_str()
        .ok_or_else(|| bad("backup log format missing"))?;
    if !["trace", "debug", "info", "warn", "error"].contains(&level.as_str())
        || !["text", "json"].contains(&format)
    {
        return Err(bad("unsupported backup logging settings"));
    }
    let quote = |s: &str| serde_json::to_string(s).map_err(err);
    let paths = roots
        .values()
        .map(|p| quote(p.to_str().ok_or_else(|| bad("non UTF8 root"))?))
        .collect::<io::Result<Vec<_>>>()?
        .join(",");
    let root_section = if roots.is_empty() {
        String::new()
    } else {
        format!("[roots]\nallowed_bases=[{paths}]\n")
    };
    Ok(format!(
        "[server]\nlisten={}\n[state]\ndir={}\n{}[log]\nlevel={}\nformat={}\n",
        quote(listen)?,
        quote(state.to_str().ok_or_else(|| bad("non UTF8 state"))?)?,
        root_section,
        quote(&level)?,
        quote(format)?
    ))
}
fn execute(args: &Args) -> io::Result<Value> {
    let state = Target::open(&args.state)?;
    // 锁之前不先判空：在线实例应确定地报告锁占用，不让操作进入目标写入。
    let lock = if state
        .dir
        .identity_at(Path::new(state_lock::LOCK_FILE_NAME))
        .map_err(err)?
        .is_some()
    {
        let f = state
            .dir
            .open_read(Path::new(state_lock::LOCK_FILE_NAME))
            .map_err(err)?;
        if !f.file.metadata()?.is_file() || f.identity.nlink != 1 || f.identity.size != 0 {
            return Err(bad("invalid state lock file"));
        }
        f.file
    } else {
        state
            .dir
            .create_exclusive(Path::new(state_lock::LOCK_FILE_NAME), 0o600)
            .map_err(err)?
    };
    let _lock = StateLock::try_acquire_file(lock, args.state.join(state_lock::LOCK_FILE_NAME))
        .map_err(|e| match e {
            StateLockError::Busy { .. } => io::Error::new(io::ErrorKind::WouldBlock, e),
            _ => err(e),
        })?;
    state_lock::ensure_restore_complete(&args.state)?;
    state.empty_except(&[state_lock::LOCK_FILE_NAME])?;
    let metadata = std::fs::metadata(state.dir.proc_fd_path())?;
    if metadata.uid() != vaultsyncd_platform_linux_server::ipc::effective_uid()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(bad(
            "empty restore state must be owned by service UID with mode 0700",
        ));
    }
    let mut source = Source::open(&args.archive)?;
    let contract = include_str!("../../../api/openapi.yaml");
    let protocol = contract
        .lines()
        .find_map(|l| l.trim().strip_prefix("version:"))
        .ok_or_else(|| bad("contract version unavailable"))?
        .trim()
        .trim_matches('"');
    restore::compatible(
        &source.value,
        &source.index,
        Compatibility {
            minimum_schema: 12,
            maximum_schema: vaultsyncd_server_store::db::target_schema_version(),
            protocol,
        },
    )
    .map_err(err)?;
    let mappings = restore::mappings(
        &source.index,
        &source.value,
        &args.state,
        &args.archive,
        &args.roots,
    )
    .map_err(err)?;
    let mut targets = BTreeMap::new();
    let mut available = state.available()?;
    for (root, path) in &mappings {
        let target = Target::open(path)?;
        target.empty_except(&[])?;
        let a = admission::storage_identity(&state.facts)
            .ok_or_else(|| bad("state storage identity missing"))?;
        let b = admission::storage_identity(&target.facts)
            .ok_or_else(|| bad("Root storage identity missing"))?;
        if a.fsid != b.fsid
            || admission::same_directory(&state.facts, &target.facts)
            || targets
                .values()
                .any(|other: &Target| admission::same_directory(&other.facts, &target.facts))
        {
            return Err(bad(
                "restore Root storage aliases or differs from state filesystem",
            ));
        }
        available = available.min(target.available()?);
        targets.insert(*root, target);
    }
    let inspect_bytes = source
        .index
        .item("database.sqlite")
        .ok_or_else(|| bad("database missing"))?
        .size;
    restore::space(
        inspect_bytes.saturating_mul(2).saturating_add(1024 * 1024),
        available,
    )
    .map_err(err)?;
    let mut scratch = Scratch::new(&state.dir)?;
    source.stream(|item| {
        if ["database.sqlite", "config.json"].contains(&item.path.as_str()) {
            Ok(Some(create(&scratch.dir, Path::new(&item.path), 0o600)?))
        } else {
            Ok(None)
        }
    })?;
    let plan = vaultsyncd_server_store::restore::plan(
        &scratch.dir.proc_fd_path().join("database.sqlite"),
        &source.index,
    )?;
    let required = restore::required(&source.index, &plan.work).map_err(err)?;
    restore::space(required, available).map_err(err)?;
    let settings: Value = serde_json::from_slice(&read_limited(
        scratch
            .dir
            .open_read(Path::new("config.json"))
            .map_err(err)?
            .file,
        1024 * 1024,
    )?)
    .map_err(err)?;
    let configuration = config(&settings, &args.state, &mappings)?;
    let mut result = json!({"state":"validated","backup_id":source.index.id.to_hex(),"server_id":source.index.server.to_hex(),"manifest_hash":ContentDigest::from_bytes(source.index.manifest_hash).to_hex(),"root_count":mappings.len().to_string(),"working_items":plan.work.len().to_string(),"required_bytes":required.to_string(),"available_bytes":available.to_string(),"external_uncollected_writes":"excluded"});
    if !args.apply {
        scratch.clean()?;
        return Ok(result);
    }
    for target in targets.values() {
        target.empty_except(&[])?;
        target.verify_identity()?;
    }
    state.verify_identity()?;
    let epoch = fresh_epoch(&plan);
    result["epoch"] = epoch.to_string().into();
    result["state"] = "incomplete".into();
    result["root_mapping"] = serde_json::to_value(
        mappings
            .iter()
            .map(|(r, p)| (r.to_hex(), p.to_string_lossy().into_owned()))
            .collect::<BTreeMap<_, _>>(),
    )
    .map_err(err)?;
    write_document(
        &state.dir,
        RESTORE_INCOMPLETE,
        &serde_json::to_vec_pretty(&result).map_err(err)?,
    )?;
    // 正式名称先放种类护栏，旧服务既有的ForeignDatabase门也会拒绝半恢复。
    drop(create(&state.dir, Path::new("server.sqlite"), 0o600)?);
    vaultsyncd_server_store::restore::guard(
        &state.dir.proc_fd_path().join("server.sqlite"),
        &source.index,
    )?;
    state.dir.fsync_dir().map_err(err)?;
    let guard_identity = state
        .dir
        .identity_at(Path::new("server.sqlite"))
        .map_err(err)?
        .ok_or_else(|| err("restore guard missing"))?;
    state.dir.mkdir(Path::new("objects"), 0o700).map_err(err)?;
    let objects = state.dir.child_dir(Path::new("objects")).map_err(err)?;
    source.stream(|item| {
        if item.path == "database.sqlite" {
            return Ok(Some(create(
                &state.dir,
                Path::new("restore.candidate.sqlite"),
                0o600,
            )?));
        }
        if item.path == "config.json" {
            return Ok(Some(create(
                &state.dir,
                Path::new("backup-config.json"),
                0o600,
            )?));
        }
        let path = ObjectLayout::relative_object_path(&ContentDigest::from_bytes(item.hash));
        if objects.identity_at(&path).map_err(err)?.is_some() {
            check_file(&objects, &path, item.hash, item.size)?;
            return Ok(None);
        }
        Ok(Some(create(&objects, &path, 0o400)?))
    })?;
    for work in &plan.work {
        let target = targets
            .get(&work.root)
            .ok_or_else(|| bad("Root mapping vanished"))?;
        if let Some((hash, size)) = work.content {
            let mut input = objects
                .open_read(&ObjectLayout::relative_object_path(
                    &ContentDigest::from_bytes(hash),
                ))
                .map_err(err)?
                .file;
            let mut output = create(&target.dir, &work.path, 0o600)?;
            io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            check_file(&target.dir, &work.path, hash, size)?;
        } else {
            target.dir.mkdir(&work.path, 0o700).map_err(err)?;
        }
    }
    let remaps: Vec<_> = targets
        .iter()
        .map(|(root, target)| Remap {
            root: *root,
            path: mappings[root].to_string_lossy().into_owned(),
            volume: admission::fsid_text(
                &admission::storage_identity(&target.facts)
                    .expect("verified btrfs")
                    .fsid,
            )
            .into_bytes(),
            identity: RootSnapshot::of_facts(&target.facts).to_spec().into_bytes(),
        })
        .collect();
    vaultsyncd_server_store::restore::apply(
        &state.dir.proc_fd_path().join("restore.candidate.sqlite"),
        &source.index,
        &remaps,
        epoch,
    )?;
    write_document(&state.dir, "server.toml", configuration.as_bytes())?;
    for (root, target) in &targets {
        let work: Vec<_> = plan
            .work
            .iter()
            .filter(|w| w.root == *root)
            .cloned()
            .collect();
        verify_work(target, &work)?;
        sync_tree(&target.dir)?;
    }
    for item in source
        .index
        .items
        .iter()
        .filter(|i| i.path.starts_with("objects/"))
    {
        check_file(
            &objects,
            &ObjectLayout::relative_object_path(&ContentDigest::from_bytes(item.hash)),
            item.hash,
            item.size,
        )?;
    }
    sync_tree(&objects)?;
    scratch.clean()?;
    state.verify_identity()?;
    result["state"] = "restored".into();
    result["config_path"] = args
        .state
        .join("server.toml")
        .to_string_lossy()
        .into_owned()
        .into();
    // 只能替换本轮已持久建立且从未变化的护栏，候选DB此时已完成全部物理/逻辑核验。
    if state
        .dir
        .identity_at(Path::new("server.sqlite"))
        .map_err(err)?
        != Some(guard_identity)
    {
        return Err(err("restore guard changed; preserved"));
    }
    state
        .dir
        .rename(
            Path::new("restore.candidate.sqlite"),
            &state.dir,
            Path::new("server.sqlite"),
            false,
        )
        .map_err(err)?;
    state.dir.fsync_dir().map_err(err)?;
    write_document(
        &state.dir,
        "restore.complete.json",
        &serde_json::to_vec_pretty(&result).map_err(err)?,
    )?;
    state
        .dir
        .unlink(Path::new(RESTORE_INCOMPLETE))
        .map_err(err)?;
    state.dir.fsync_dir().map_err(err)?;
    Ok(result)
}
pub async fn run(args: Vec<OsString>) -> u8 {
    let args = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    let json = args.json;
    let result = tokio::task::spawn_blocking(move || execute(&args))
        .await
        .map_err(err)
        .and_then(|r| r);
    match result {
        Ok(v) => {
            if json {
                println!("{v}")
            } else {
                println!("{}", serde_json::to_string_pretty(&v).expect("JSON"))
            }
            0
        }
        Err(e) => {
            if json {
                println!("{}", json!({"state":"incomplete","error":e.to_string()}))
            } else {
                eprintln!("restore failed: {e}")
            }
            if e.kind() == io::ErrorKind::WouldBlock {
                3
            } else {
                1
            }
        }
    }
}
