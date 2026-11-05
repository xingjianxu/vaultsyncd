//! S013 存储准入原型命令：把挂载准入的判定过程与结论打印成稳定的 `key=value` 行。
//!
//! 用法见 `tests/platform/linux/` 下的脚本；这是原型诊断入口，不是产品 CLI（产品入口在
//! `apps/server` 与管理面，按 design §5.3/§5.7 由后续卡实现）。
//!
//! 退出码：`0` 接受/满足；`3` 拒绝或不满足；`4` 结论未定（扫描上限内无法证明合规）；
//! `1` 探测或 I/O 错误；`2` 用法错误。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vaultsyncd_platform_linux_server::mounts::admission::BackendKind;
use vaultsyncd_platform_linux_server::mounts::admission::{
    self, AdmissionDecision, AdmissionInput, fsid_text,
};
use vaultsyncd_platform_linux_server::mounts::content::{self, PLACEHOLDER_EVIDENCE_NONE};
use vaultsyncd_platform_linux_server::mounts::mountinfo;
use vaultsyncd_platform_linux_server::mounts::probe::{
    self, ScanCompleteness, ScanPolicy, ScanReport,
};

const EXIT_ACCEPTED: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REJECTED: u8 = 3;
const EXIT_INCONCLUSIVE: u8 = 4;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((subcommand, rest)) = args.split_first() else {
        return usage("缺少子命令");
    };
    match subcommand.as_str() {
        "inspect" => inspect(rest),
        "identity" => identity(rest),
        "same-storage" => same_storage(rest),
        "content" => content(rest),
        other => usage(&format!("未知子命令 '{other}'")),
    }
}

fn usage(message: &str) -> ExitCode {
    eprintln!("{message}");
    eprintln!(
        "用法：storage-admission <inspect PATH [--internal-state-dir PATH]... \
         [--registered-root PATH]... [--max-scan-entries N] [--max-scan-depth N] | \
         identity PATH | same-storage PATH_A PATH_B | content PATH>"
    );
    ExitCode::from(EXIT_USAGE)
}

fn error(stage: &str, message: impl std::fmt::Display) -> ExitCode {
    println!("error={stage}: {message}");
    eprintln!("storage-admission: {stage}: {message}");
    ExitCode::from(EXIT_ERROR)
}

struct InspectArgs {
    path: PathBuf,
    internal_state_dirs: Vec<PathBuf>,
    registered_roots: Vec<PathBuf>,
    policy: ScanPolicy,
}

fn parse_inspect_args(rest: &[String]) -> Result<InspectArgs, String> {
    let mut path: Option<PathBuf> = None;
    let mut internal_state_dirs = Vec::new();
    let mut registered_roots = Vec::new();
    let mut policy = ScanPolicy::default();
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--internal-state-dir"
            | "--registered-root"
            | "--max-scan-entries"
            | "--max-scan-depth" => {
                let flag = rest[index].clone();
                let value = rest
                    .get(index + 1)
                    .ok_or_else(|| format!("{flag} 缺少取值"))?;
                index += 2;
                match flag.as_str() {
                    "--internal-state-dir" => internal_state_dirs.push(PathBuf::from(value)),
                    "--registered-root" => registered_roots.push(PathBuf::from(value)),
                    "--max-scan-entries" => policy.max_entries = parse_u64(value, &flag)?,
                    _ => {
                        policy.max_depth = parse_u64(value, &flag)?
                            .try_into()
                            .map_err(|_| format!("{flag} 的取值 {value} 超出 u32 范围"))?
                    }
                }
            }
            other if other.starts_with("--") => return Err(format!("未知参数 '{other}'")),
            positional => {
                if path.is_some() {
                    return Err(format!("多余的路径参数 '{positional}'"));
                }
                path = Some(PathBuf::from(positional));
                index += 1;
            }
        }
    }
    Ok(InspectArgs {
        path: path.ok_or_else(|| "inspect 需要一个路径".to_owned())?,
        internal_state_dirs,
        registered_roots,
        policy,
    })
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|error| format!("{flag} 的取值 '{value}' 不是无符号整数：{error}"))
}

fn single_path<'a>(rest: &'a [String], subcommand: &str) -> Result<&'a Path, String> {
    match rest {
        [path] => Ok(Path::new(path)),
        _ => Err(format!("{subcommand} 需要且只需要一个路径参数")),
    }
}

fn inspect(rest: &[String]) -> ExitCode {
    let args = match parse_inspect_args(rest) {
        Ok(args) => args,
        Err(message) => return usage(&message),
    };

    let root = match probe::probe_path(&args.path) {
        Ok(facts) => facts,
        Err(error) => return self::error("probe.root", error),
    };
    let mut internal_state_dirs = Vec::new();
    for path in &args.internal_state_dirs {
        match probe::probe_path(path) {
            Ok(facts) => internal_state_dirs.push(facts),
            Err(error) => return self::error("probe.internal_state_dir", error),
        }
    }
    let mut registered_roots = Vec::new();
    for path in &args.registered_roots {
        match probe::probe_path(path) {
            Ok(facts) => registered_roots.push(facts),
            Err(error) => return self::error("probe.registered_root", error),
        }
    }
    let mounts = match mountinfo::read_self() {
        Ok(mounts) => mounts,
        Err(error) => return self::error("mountinfo", error),
    };
    // 只有 btrfs Root 需要子卷/NOCOW 遍历；其它后端在第一步就会被拒绝，遍历也没有意义
    // （非 btrfs 上没有子卷身份，GETFLAGS 也未必支持）。
    let backend = admission::classify_backend(root.fs_magic, &root.fs_type);
    let scanned = backend == BackendKind::Btrfs;
    let scan = if scanned {
        let Some(subvolume_id) = root.btrfs.as_ref().and_then(|btrfs| btrfs.subvolume_id) else {
            return self::error(
                "scan",
                "Root 是 btrfs 但无法解析子卷 ID，无法遍历确认嵌套子卷",
            );
        };
        match probe::scan_root(&root.resolved_path, subvolume_id, &args.policy) {
            Ok(scan) => scan,
            Err(error) => return self::error("scan", error),
        }
    } else {
        ScanReport {
            subvolumes: Vec::new(),
            nocow_directories: Vec::new(),
            entries_scanned: 0,
            completeness: ScanCompleteness::Complete,
        }
    };

    print_facts(&root);
    print_scan(&scan, scanned);
    let sub_mounts = admission::nested_mounts(&mounts, &root);
    println!("submounts={}", sub_mounts.len());
    for entry in &sub_mounts {
        println!(
            "submount={} fstype={} mount_id={}",
            entry.mount_point.display(),
            entry.fs_type,
            entry.mount_id
        );
    }

    let outcome = admission::evaluate(&AdmissionInput {
        root: &root,
        internal_state_dirs: &internal_state_dirs,
        registered_roots: &registered_roots,
        mounts: &mounts,
        scan: &scan,
    });
    println!("decision={}", outcome.decision.as_str());
    match &outcome.decision {
        AdmissionDecision::Accepted => {
            println!("reason=-");
            println!("error_code=-");
            println!("detail=-");
            for note in &outcome.notes {
                println!("note={note}");
            }
            ExitCode::from(EXIT_ACCEPTED)
        }
        AdmissionDecision::Rejected(rejection) => {
            println!("reason={}", rejection.code());
            println!("error_code={}", rejection.design_error_code());
            println!("detail={}", rejection.detail());
            for note in &outcome.notes {
                println!("note={note}");
            }
            ExitCode::from(EXIT_REJECTED)
        }
        AdmissionDecision::Inconclusive(reason) => {
            println!("reason={}", reason.code());
            println!("error_code=-");
            println!("detail=扫描上限内无法证明 Root 合规，结论未定");
            for note in &outcome.notes {
                println!("note={note}");
            }
            ExitCode::from(EXIT_INCONCLUSIVE)
        }
    }
}

fn print_facts(facts: &probe::StorageFacts) {
    println!("path={}", facts.given_path.display());
    println!("resolved={}", facts.resolved_path.display());
    println!(
        "backend={}",
        admission::classify_backend(facts.fs_magic, &facts.fs_type).as_str()
    );
    println!("fstype={}", facts.fs_type);
    println!("fs_magic={:#x}", facts.fs_magic);
    println!(
        "statfs_fsid={:08x}:{:08x}",
        facts.statfs_fsid[0], facts.statfs_fsid[1]
    );
    println!("mount_id={}", facts.mount.mount_id);
    println!("mount_id_source={}", facts.mount_id_source.as_str());
    println!("mount_point={}", facts.mount.mount_point.display());
    println!("mount_root={}", facts.mount.root.display());
    println!("statx_device={}", facts.device);
    println!("mount_device={}", facts.mount.device);
    println!("mount_options={}", facts.mount.mount_options.join(","));
    println!("super_options={}", facts.mount.super_options.join(","));
    println!("inode={}", facts.inode);
    match facts.btime {
        Some((seconds, nanos)) => println!("btime={seconds}.{nanos:09}"),
        None => println!("btime=-"),
    }
    print_btrfs(facts);
    match facts.inode_flags {
        Some(flags) => {
            println!("inode_flags={flags:#010x}");
            println!("nocow={}", admission::has_nocow_flag(flags));
        }
        None => {
            println!("inode_flags=-");
            println!("nocow=unknown");
            println!(
                "inode_flags_error={}",
                facts.inode_flags_error.as_deref().unwrap_or("unknown")
            );
        }
    }
}

fn print_btrfs(facts: &probe::StorageFacts) {
    match &facts.btrfs {
        Some(btrfs) => {
            println!("fsid={}", fsid_text(&btrfs.fsid));
            match btrfs.subvolume_id {
                Some(id) => println!("subvol_id={id}"),
                None => println!("subvol_id=-"),
            }
            match btrfs.subvolume_id_source {
                Some(source) => println!("subvol_id_source={}", source.as_str()),
                None => println!("subvol_id_source=-"),
            }
            if let Some(error) = &btrfs.subvolume_id_error {
                println!("subvol_id_error={error}");
            }
        }
        None => {
            println!("fsid=-");
            println!("subvol_id=-");
            println!("subvol_id_source=-");
        }
    }
}

fn print_scan(scan: &ScanReport, scanned: bool) {
    println!("scan_entries={}", scan.entries_scanned);
    if !scanned {
        println!("scan=skipped:backend_not_btrfs");
        println!("nested_subvolumes=0");
        println!("nocow_directories=0");
        return;
    }
    println!(
        "scan={}",
        match &scan.completeness {
            ScanCompleteness::Complete => "complete".to_owned(),
            ScanCompleteness::DepthLimitReached { relative_path } => {
                format!("depth_limit_reached:{}", relative_path.display())
            }
            ScanCompleteness::EntryLimitReached { relative_path } => {
                format!("entry_limit_reached:{}", relative_path.display())
            }
        }
    );
    println!("nested_subvolumes={}", scan.subvolumes.len());
    for nested in &scan.subvolumes {
        println!(
            "nested_subvolume={} subvol_id={}",
            nested.relative_path.display(),
            nested.subvolume_id
        );
    }
    println!("nocow_directories={}", scan.nocow_directories.len());
    for nocow in &scan.nocow_directories {
        println!(
            "nocow_directory={} flags={:#010x}",
            nocow.relative_path.display(),
            nocow.flags
        );
    }
}

fn identity(rest: &[String]) -> ExitCode {
    let path = match single_path(rest, "identity") {
        Ok(path) => path,
        Err(message) => return usage(&message),
    };
    let facts = match probe::probe_path(path) {
        Ok(facts) => facts,
        Err(error) => return self::error("probe", error),
    };
    println!("path={}", facts.given_path.display());
    println!("resolved={}", facts.resolved_path.display());
    println!(
        "backend={}",
        admission::classify_backend(facts.fs_magic, &facts.fs_type).as_str()
    );
    println!("fstype={}", facts.fs_type);
    println!("mount_id={}", facts.mount.mount_id);
    println!("statx_device={}", facts.device);
    println!("mount_device={}", facts.mount.device);
    println!("inode={}", facts.inode);
    print_btrfs(&facts);
    ExitCode::from(EXIT_ACCEPTED)
}

fn same_storage(rest: &[String]) -> ExitCode {
    let [left, right] = rest else {
        return usage("same-storage 需要两个路径参数");
    };
    let left_facts = match probe::probe_path(Path::new(left)) {
        Ok(facts) => facts,
        Err(error) => return self::error("probe.left", error),
    };
    let right_facts = match probe::probe_path(Path::new(right)) {
        Ok(facts) => facts,
        Err(error) => return self::error("probe.right", error),
    };
    let same = admission::same_storage(&left_facts, &right_facts);
    println!("same_storage={same}");
    println!(
        "same_directory={}",
        admission::same_directory(&left_facts, &right_facts)
    );
    println!("left_mount_id={}", left_facts.mount.mount_id);
    println!("right_mount_id={}", right_facts.mount.mount_id);
    if same {
        ExitCode::from(EXIT_ACCEPTED)
    } else {
        ExitCode::from(EXIT_REJECTED)
    }
}

fn content(rest: &[String]) -> ExitCode {
    let path = match single_path(rest, "content") {
        Ok(path) => path,
        Err(message) => return usage(&message),
    };
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return self::error("metadata", error),
    };
    use std::os::unix::fs::MetadataExt;
    let logical_size = metadata.len();
    let allocated_bytes = metadata.blocks().saturating_mul(512);
    let shape = content::classify_content_shape(logical_size, allocated_bytes);
    println!("path={}", path.display());
    println!(
        "file_type={}",
        if metadata.is_dir() { "dir" } else { "file" }
    );
    println!("logical_size={logical_size}");
    println!("allocated_bytes={allocated_bytes}");
    println!("content_shape={}", shape.as_str());
    match probe::inode_flags_of_path(path) {
        Ok(flags) => {
            println!("inode_flags={flags:#010x}");
            println!("nocow={}", admission::has_nocow_flag(flags));
        }
        Err(error) => println!("inode_flags=- ({error})"),
    }
    println!("placeholder_evidence={PLACEHOLDER_EVIDENCE_NONE}");
    println!("placeholder_note=稀疏/未分配块不是占位证据；第三方占位格式无统一判定");
    ExitCode::from(EXIT_ACCEPTED)
}
