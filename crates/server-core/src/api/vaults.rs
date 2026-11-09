//! Vault 注册与健康状态的用例规则（design §11.4、§15.2）。
//!
//! 边界：本模块只做判定与编码，不做文件系统 I/O、不读数据库。挂载探测（后端类型、子卷、NOCOW、
//! 嵌套挂载、身份字节）由 `crates/platform-linux-server` 的 `mounts::admission` 给出，注册行由
//! `crates/server-store` 的 `vaults::insert_vault` 写入，HTTP 处理在 `apps/server`。
//!
//! 不变量：
//!
//! - **只接受预配置允许基目录中的目录**：允许基目录是部署契约（管理员在配置里给出），不是客户端
//!   能自选的路径。判定按路径分量，不用字符串前缀（design §16.1）：`/data/vault` 不是
//!   `/data/vaultx` 的祖先。规范路径也要再判一次——第一段判的是客户端给的文本，探测之后才知道
//!   真实目录在哪，符号链接不能把它带出允许范围。
//! - **配置阶段拒绝一律 `UNSUPPORTED_STORAGE_BACKEND`**（design §11.4）：拒绝发生在注册时，
//!   因此不用运行期的 `VAULT_UNAVAILABLE`；越出允许基目录是授权问题，用 `ACCESS_DENIED`，且不
//!   回显“允许哪些目录”（那等于把服务器布局告诉调用方）。
//! - **不猜结论**：探测不完整（扫描上限内无法证明合规）不是通过，也不是拒绝，单独
//!   [`RegistrationVerdict::Inconclusive`]，调用方不得据此写库。
//! - **注册不碰目录内容**：本模块与注册流程只读探测，不改写、不清空、不导入既有文件（design
//!   §11.4“不尝试把不支持目录自动转换、下载填充或清空”）；初始导入由后续的采集步骤完成，注册
//!   结果里只报“待导入”状态。

use std::path::{Component, Path, PathBuf};

/// 允许基目录配置有误（部署契约写错就不能服务，不能默默放宽）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllowedBaseError {
    /// 一个允许基目录都没配；空集合等于“任何路径都拒绝”，那是配置错误而不是策略。
    Empty,
    /// 基目录不是服务器绝对路径。
    NotAbsolute {
        /// 配置里给出的路径。
        path: PathBuf,
    },
    /// 基目录含 `.`/`..` 或空分量：注册判定按分量比较，规范化形式是前提（不在本层折叠）。
    NotNormalized {
        /// 配置里给出的路径。
        path: PathBuf,
    },
}

impl std::fmt::Display for AllowedBaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("没有配置任何允许的 Vault 基目录"),
            Self::NotAbsolute { path } => {
                write!(f, "允许基目录 {} 不是绝对路径", path.display())
            }
            Self::NotNormalized { path } => {
                write!(f, "允许基目录 {} 含 . 或 .. 分量", path.display())
            }
        }
    }
}

impl std::error::Error for AllowedBaseError {}

/// 预配置的允许基目录集合（design §11.4“确认目录在允许基目录内”）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedBases {
    bases: Vec<PathBuf>,
}

impl AllowedBases {
    /// 用规范化绝对路径建立集合；空集合、相对路径与含 `.`/`..` 的路径都拒绝。
    ///
    /// # Errors
    ///
    /// 见 [`AllowedBaseError`]。
    pub fn new(bases: impl IntoIterator<Item = PathBuf>) -> Result<Self, AllowedBaseError> {
        let mut kept: Vec<PathBuf> = Vec::new();
        for path in bases {
            require_normal_absolute(&path).map_err(|reason| match reason {
                NormalPathError::NotAbsolute => {
                    AllowedBaseError::NotAbsolute { path: path.clone() }
                }
                NormalPathError::NotNormalized => {
                    AllowedBaseError::NotNormalized { path: path.clone() }
                }
            })?;
            if !kept.contains(&path) {
                kept.push(path);
            }
        }
        if kept.is_empty() {
            return Err(AllowedBaseError::Empty);
        }
        Ok(Self { bases: kept })
    }

    /// 路径是否落在某个允许基目录内（含基目录本身）。
    ///
    /// 判定前提是被比较的路径也是服务器绝对路径；相对路径、含 `.`/`..` 的路径一律不在允许范围内
    /// —— 本层不折叠路径，否则 `/data/../../etc` 会被当成 `/etc` 的兄弟。
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        if require_normal_absolute(path).is_err() {
            return false;
        }
        self.bases
            .iter()
            .any(|base| base_is_ancestor_or_self(base, path))
    }

    /// 管理员通道枚举预配置基目录；普通客户端不得暴露。
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.bases.iter().map(PathBuf::as_path)
    }

    /// 配置了几个（去重后的）基目录。
    #[must_use]
    pub fn len(&self) -> usize {
        self.bases.len()
    }

    /// 是否为空（[`AllowedBases::new`] 不会建立空集合，保留给调用方判空）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bases.is_empty()
    }
}

/// 平台层对候选目录的探测结论（核心不重复做挂载判定）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateFacts {
    /// 普通目录，且存储准入通过：两个身份字节由平台层编码，核心原样存。
    Accepted {
        /// 卷身份（btrfs FSID 的 UUID 文本字节）。
        volume_identity: Vec<u8>,
        /// 根目录身份（平台快照文本字节）。
        vault_identity: Vec<u8>,
    },
    /// 不是可注册的普通目录（不存在、不是目录、打不开、无权限、打开期被替换）。
    NotADirectory {
        /// 平台短码。
        reason: String,
        /// 人读细节。
        detail: String,
    },
    /// 存储准入拒绝：FUSE/占位视图、非 btrfs、NOCOW、嵌套子卷或子挂载、内部目录重叠、已注册别名
    /// 或路径重叠（design §11.4 的配置阶段拒绝）。
    UnsupportedStorage {
        /// 平台短码（`AdmissionRejection::code`）。
        reason: String,
        /// 人读细节。
        detail: String,
    },
    /// 无法给出结论：扫描上限内不能证明合规。
    Inconclusive {
        /// 平台短码。
        reason: String,
        /// 人读细节。
        detail: String,
    },
}

/// 注册失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationRejection {
    /// 路径不在任何允许基目录内（含相对路径与含 `..` 的路径）。
    OutsideAllowedBases,
    /// 不是可注册的普通目录。
    NotADirectory {
        /// 平台短码。
        reason: String,
        /// 人读细节。
        detail: String,
    },
    /// 存储准入拒绝（配置阶段的设计错误码）。
    UnsupportedStorage {
        /// 平台短码。
        reason: String,
        /// 人读细节。
        detail: String,
    },
    /// 探测不完整，不能注册也不能断言目录不合规。
    Inconclusive {
        /// 平台短码。
        reason: String,
        /// 人读细节。
        detail: String,
    },
}

impl RegistrationRejection {
    /// 对外错误码（design §15.4 的闭集）。
    ///
    /// 越出允许基目录是授权问题（`ACCESS_DENIED`）；其余都是 design §11.4 的配置阶段拒绝，
    /// 统一 `UNSUPPORTED_STORAGE_BACKEND`——包括“不是目录”，因为客户端要做的事是同一个：不要
    /// 把这个服务器目录配进来。
    #[must_use]
    pub fn design_error_code(&self) -> &'static str {
        match self {
            Self::OutsideAllowedBases => "ACCESS_DENIED",
            Self::NotADirectory { .. }
            | Self::UnsupportedStorage { .. }
            | Self::Inconclusive { .. } => "UNSUPPORTED_STORAGE_BACKEND",
        }
    }

    /// HTTP 状态码（与 [`Self::design_error_code`] 对应）。
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::OutsideAllowedBases => 403,
            Self::NotADirectory { .. }
            | Self::UnsupportedStorage { .. }
            | Self::Inconclusive { .. } => 422,
        }
    }

    /// 结构化原因短码（错误信封的 `details.reason`）。
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::OutsideAllowedBases => "outside_allowed_bases",
            Self::NotADirectory { reason, .. }
            | Self::UnsupportedStorage { reason, .. }
            | Self::Inconclusive { reason, .. } => reason,
        }
    }

    /// 人读细节。
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::OutsideAllowedBases => "路径不在服务端预配置的允许基目录内",
            Self::NotADirectory { detail, .. }
            | Self::UnsupportedStorage { detail, .. }
            | Self::Inconclusive { detail, .. } => detail,
        }
    }

    /// 同一请求原样重试是否可能成功：配置问题不会因为重试而改变。
    #[must_use]
    pub fn retryable(&self) -> bool {
        false
    }
}

/// 注册判定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrationVerdict {
    /// 可以注册，附平台给的两个身份字节。
    Accepted {
        /// 卷身份。
        volume_identity: Vec<u8>,
        /// 根目录身份。
        vault_identity: Vec<u8>,
    },
    /// 不能注册。
    Rejected(RegistrationRejection),
}

impl RegistrationVerdict {
    /// 判定是否通过。
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }
}

/// 判定一个候选目录能否注册（design §11.4）。
///
/// `resolved` 是平台层探测后得到的规范路径，不是客户端给的原文：第一段按原文判允许基目录是为了
/// 尽早拒绝、不探测任意路径，这一段再判规范路径是为了让符号链接不能把 Vault 带出允许范围。
#[must_use]
pub fn evaluate_registration(
    bases: &AllowedBases,
    resolved: &Path,
    facts: CandidateFacts,
) -> RegistrationVerdict {
    if !bases.contains(resolved) {
        return RegistrationVerdict::Rejected(RegistrationRejection::OutsideAllowedBases);
    }
    match facts {
        CandidateFacts::Accepted {
            volume_identity,
            vault_identity,
        } => RegistrationVerdict::Accepted {
            volume_identity,
            vault_identity,
        },
        CandidateFacts::NotADirectory { reason, detail } => {
            RegistrationVerdict::Rejected(RegistrationRejection::NotADirectory { reason, detail })
        }
        CandidateFacts::UnsupportedStorage { reason, detail } => {
            RegistrationVerdict::Rejected(RegistrationRejection::UnsupportedStorage {
                reason,
                detail,
            })
        }
        CandidateFacts::Inconclusive { reason, detail } => {
            RegistrationVerdict::Rejected(RegistrationRejection::Inconclusive { reason, detail })
        }
    }
}

/// 路径形式错误的两种原因。
enum NormalPathError {
    NotAbsolute,
    NotNormalized,
}

/// 要求“服务器绝对路径 + 无 `.`/`..`/空分量”，不做任何折叠。
fn require_normal_absolute(path: &Path) -> Result<(), NormalPathError> {
    let mut components = path.components();
    match components.next() {
        Some(Component::RootDir) => {}
        _ => return Err(NormalPathError::NotAbsolute),
    }
    for component in components {
        match component {
            Component::Normal(_) => {}
            _ => return Err(NormalPathError::NotNormalized),
        }
    }
    Ok(())
}

/// 按分量判断 `base` 是 `path` 的祖先或本身（不用字符串前缀：`/data/vaultx` 不在 `/data/vault` 下）。
fn base_is_ancestor_or_self(base: &Path, path: &Path) -> bool {
    let base_parts: Vec<Component<'_>> = base.components().collect();
    let path_parts: Vec<Component<'_>> = path.components().collect();
    if base_parts.len() > path_parts.len() {
        return false;
    }
    base_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bases(entries: &[&str]) -> AllowedBases {
        AllowedBases::new(entries.iter().map(PathBuf::from)).expect("允许基目录")
    }

    fn accepted() -> CandidateFacts {
        CandidateFacts::Accepted {
            volume_identity: b"fsid".to_vec(),
            vault_identity: b"vault".to_vec(),
        }
    }

    #[test]
    fn empty_or_non_normal_bases_are_config_errors() {
        assert_eq!(
            AllowedBases::new(Vec::new()).unwrap_err(),
            AllowedBaseError::Empty
        );
        assert_eq!(
            AllowedBases::new([PathBuf::from("relative/dir")]).unwrap_err(),
            AllowedBaseError::NotAbsolute {
                path: PathBuf::from("relative/dir")
            }
        );
        assert_eq!(
            AllowedBases::new([PathBuf::from("/data/../data")]).unwrap_err(),
            AllowedBaseError::NotNormalized {
                path: PathBuf::from("/data/../data")
            }
        );
        // `.` 与结尾分隔符不是 `..` 分量：`Components` 会折叠它们，不影响判定。
        let trailing = bases(&["/data/vaults/"]);
        assert!(trailing.contains(Path::new("/data/vaults/a")));
    }

    #[test]
    fn duplicate_bases_are_counted_once() {
        let bases = bases(&["/data/vaults", "/data/vaults", "/srv/vaults"]);
        assert_eq!(bases.len(), 2);
        assert!(!bases.is_empty());
    }

    #[test]
    fn containment_is_component_wise_not_string_prefix() {
        let bases = bases(&["/data/vaults"]);
        assert!(bases.contains(Path::new("/data/vaults")), "基目录本身可用");
        assert!(bases.contains(Path::new("/data/vaults/a")));
        assert!(bases.contains(Path::new("/data/vaults/a/b")));
        assert!(
            !bases.contains(Path::new("/data/vaultsx")),
            "同前缀兄弟不是子目录"
        );
        assert!(!bases.contains(Path::new("/data")), "父目录不在允许范围内");
        assert!(!bases.contains(Path::new("/etc")));
        assert!(
            !bases.contains(Path::new("data/vaults")),
            "相对路径不在允许范围内"
        );
        assert!(
            !bases.contains(Path::new("/data/vaults/../etc")),
            "不折叠 .."
        );
        assert!(
            !bases.contains(Path::new("/")),
            "根目录不算任何基目录的子目录"
        );
    }

    #[test]
    fn outside_bases_is_denied_without_revealing_the_layout() {
        let bases = bases(&["/data/vaults"]);
        let verdict = evaluate_registration(&bases, Path::new("/etc"), accepted());
        let RegistrationVerdict::Rejected(rejection) = verdict else {
            panic!("越界路径必须被拒绝");
        };
        assert_eq!(rejection.design_error_code(), "ACCESS_DENIED");
        assert_eq!(rejection.http_status(), 403);
        assert_eq!(rejection.reason(), "outside_allowed_bases");
        assert!(!rejection.retryable());
        assert!(
            !rejection.detail().contains("/data/vaults"),
            "拒绝理由不得回显允许基目录：{}",
            rejection.detail()
        );
    }

    #[test]
    fn accepted_candidate_keeps_platform_identity_bytes() {
        let bases = bases(&["/data/vaults"]);
        let verdict = evaluate_registration(&bases, Path::new("/data/vaults/a"), accepted());
        assert_eq!(
            verdict,
            RegistrationVerdict::Accepted {
                volume_identity: b"fsid".to_vec(),
                vault_identity: b"vault".to_vec(),
            }
        );
        assert!(verdict.is_accepted());
    }

    #[test]
    fn storage_rejections_map_to_the_configuration_error_code() {
        let bases = bases(&["/data/vaults"]);
        let cases = [
            CandidateFacts::NotADirectory {
                reason: "not_a_directory".to_owned(),
                detail: "路径不是目录".to_owned(),
            },
            CandidateFacts::UnsupportedStorage {
                reason: "unsupported_backend".to_owned(),
                detail: "后端 fuse 不是本地 btrfs".to_owned(),
            },
            CandidateFacts::UnsupportedStorage {
                reason: "internal_state_overlap".to_owned(),
                detail: "内部状态目录与 Vault 重叠".to_owned(),
            },
            CandidateFacts::Inconclusive {
                reason: "scan_entry_limit_reached".to_owned(),
                detail: "条目上限内无法证明合规".to_owned(),
            },
        ];
        for facts in cases {
            let expected_reason = match &facts {
                CandidateFacts::NotADirectory { reason, .. }
                | CandidateFacts::UnsupportedStorage { reason, .. }
                | CandidateFacts::Inconclusive { reason, .. } => reason.clone(),
                CandidateFacts::Accepted { .. } => unreachable!(),
            };
            let verdict = evaluate_registration(&bases, Path::new("/data/vaults/a"), facts);
            let RegistrationVerdict::Rejected(rejection) = verdict else {
                panic!("非合规候选必须被拒绝");
            };
            assert_eq!(expected_reason, rejection.reason());
            assert_eq!(rejection.design_error_code(), "UNSUPPORTED_STORAGE_BACKEND");
            assert_eq!(rejection.http_status(), 422);
            assert!(!rejection.retryable());
        }
    }

    #[test]
    fn resolved_path_outside_bases_is_rejected_even_if_the_given_path_looked_inside() {
        // 客户端给的是允许基目录下的文本，探测出的规范路径却是别处（符号链接逃逸）。
        let bases = bases(&["/data/vaults"]);
        let verdict = evaluate_registration(
            &bases,
            Path::new("/data/vaults/escape/target"),
            CandidateFacts::Accepted {
                volume_identity: b"fsid".to_vec(),
                vault_identity: b"vault".to_vec(),
            },
        );
        assert!(verdict.is_accepted(), "规范路径仍在基目录内就该接受");
        let verdict = evaluate_registration(&bases, Path::new("/srv/elsewhere"), accepted());
        assert_eq!(
            verdict,
            RegistrationVerdict::Rejected(RegistrationRejection::OutsideAllowedBases)
        );
    }
}
