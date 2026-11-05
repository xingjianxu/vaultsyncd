//! 内容冲突与稳定副本身份（`docs/design.md` §10.1、§10.2、I02、I04、I06）。
//!
//! 本模块只处理 §10.1 中“两端修改内容且字节不同”这一行：服务器已提交的 head 保留主路径，
//! 迟到分支的不同内容成为**新冲突条目**，并按 §10.2 生成带来源标识的名称。它输出两份计划：
//!
//! | 计划 | 内容 |
//! | --- | --- |
//! | [`ConflictPlan::main`] | 主条目保留服务器 head（[`Action::ApplyRemote`]），替换本地字节前必须先提交冲突副本 |
//! | [`ConflictPlan::copy`] | 迟到分支内容的新冲突条目（[`Action::CreateConflictCopy`]），或同一 `OpId` 的既有结果 |
//!
//! 三条不变量决定这里为什么不能“凭名字去重”：
//!
//! 1. **胜者只看服务器提交序**：保留哪一份由 [`ServerFact::revision`] 表达，`mtime` 只随事实记录，
//!    交换两台设备的时钟不改变哪一份进入主路径（design §10.1 首段、I04）。
//! 2. **同一结果只按长度与 SHA-256 归并**：内容相同（[`ContentFingerprint`] 相等）就不创建副本，
//!    名称相同、路径相同或 `mtime` 相同都不足以归并（design §10.1 第二行）。
//! 3. **幂等依据是 `OpId`，不是文件名**：冲突条目的身份（[`EntryId`]）与路径由服务端为该 `OpId`
//!    分配一次后持久化；重试返回同一身份与原结果，不会不断新增副本（design §10.1 末行、§10.2、
//!    I06、T02/T03）。
//!
//! 名称按 §10.2 的建议形式 `<原名干>（冲突-<设备短标识>-<服务器时间>-<op 短标识>）<扩展名>` 生成：
//! 扩展名保留、时间取服务器提交时间、短标识长度可扩展，整体受目标文件系统的名称预算约束
//! （[`NameBudget`]），原名干按预算截断。名称只是给用户的标签：真正去重靠 `OpId`/冲突身份，
//! 因此短标识扩展后同一 `OpId` 得到不同文件名也不违反幂等。
//!
//! 本模块不做 I/O：不读时钟（服务器时间以值传入）、不分配身份、不枚举同级条目（目标名称是否已被
//! 占用由执行器按 [`Prerequisite::TargetPathUsable`] 核验并重新规划另名）。

use core::fmt;

use vaultsyncd_domain::ids::{EntryId, ID_HEX_LEN, OpId};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_fs_common::name::{
    MAX_COMPONENT_UTF8_BYTES, MAX_COMPONENT_UTF16_UNITS, NameIssue, validate_name,
};
use vaultsyncd_fs_common::path::{PathIssue, RelativePath};

use crate::reconcile::{
    Action, ContentFingerprint, Dependency, EntryKind, LocalFact, LocalProtection, Plan,
    Prerequisite, ServerFact, apply_remote,
};

/// 冲突名称中短标识允许的最小十六进制长度。
pub const MIN_SHORT_ID_LEN: usize = 8;

/// 冲突名称中短标识的十六进制长度（design §10.2：“短标识碰撞时扩展长度”）。
///
/// 身份是 16 字节，规范文本是 [`ID_HEX_LEN`] 个十六进制字符；这里只取前若干字符用于名称。
/// 截短后可能撞名，撞名时改用 [`ShortIdLen::extended`] 加长，直到用满整个规范文本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortIdLen(usize);

impl ShortIdLen {
    /// 建议形式的默认长度：8 个十六进制字符。
    pub const DEFAULT: Self = Self(MIN_SHORT_ID_LEN);

    /// 由长度构造；只接受 [`MIN_SHORT_ID_LEN`]—[`ID_HEX_LEN`]。
    ///
    /// # Errors
    ///
    /// 长度过短或超过 [`ID_HEX_LEN`] 时返回 [`ShortIdLenError`]。
    pub fn new(len: usize) -> Result<Self, ShortIdLenError> {
        if len < MIN_SHORT_ID_LEN {
            return Err(ShortIdLenError::TooShort {
                len,
                min: MIN_SHORT_ID_LEN,
            });
        }
        if len > ID_HEX_LEN {
            return Err(ShortIdLenError::TooLong {
                len,
                max: ID_HEX_LEN,
            });
        }
        Ok(Self(len))
    }

    /// 长度（十六进制字符数）。
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// 撞名后加长的下一档（翻倍、最多到 [`ID_HEX_LEN`]）；已用满整个规范文本时返回 `None`。
    #[must_use]
    pub fn extended(self) -> Option<Self> {
        let next = (self.0 * 2).min(ID_HEX_LEN);
        if next == self.0 {
            None
        } else {
            Some(Self(next))
        }
    }
}

/// 短标识长度不合规的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortIdLenError {
    /// 短于 [`MIN_SHORT_ID_LEN`]：太短的名字片段无法区分不同操作。
    TooShort {
        /// 请求的长度。
        len: usize,
        /// 允许的最小长度。
        min: usize,
    },
    /// 超过 [`ID_HEX_LEN`]：身份文本没有这么多字符。
    TooLong {
        /// 请求的长度。
        len: usize,
        /// 允许的最大长度。
        max: usize,
    },
}

impl fmt::Display for ShortIdLenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len, min } => write!(f, "短标识长度 {len} 小于下限 {min}"),
            Self::TooLong { len, max } => write!(f, "短标识长度 {len} 大于上限 {max}"),
        }
    }
}

impl core::error::Error for ShortIdLenError {}

/// 目标文件系统的名称预算：一个名称分量允许的最大 UTF-8 字节数与 UTF-16 码元数。
///
/// 跨端可表示性的硬上限是 [`MAX_COMPONENT_UTF8_BYTES`] / [`MAX_COMPONENT_UTF16_UNITS`]
/// （[`NameBudget::CROSS_PLATFORM`]）。目标文件系统可能更紧（例如某些网络文件系统的组件上限更小），
/// 因此由调用方给出实际预算，生成命中预算时截断原名干而不是截断后缀。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameBudget {
    max_utf8_bytes: usize,
    max_utf16_units: usize,
}

impl NameBudget {
    /// 跨端可表示性的上限：255 字节 / 255 码元。
    pub const CROSS_PLATFORM: Self = Self {
        max_utf8_bytes: MAX_COMPONENT_UTF8_BYTES,
        max_utf16_units: MAX_COMPONENT_UTF16_UNITS,
    };

    /// 由实际预算构造；两个维度都必须为正且不超过跨端上限。
    ///
    /// # Errors
    ///
    /// 任一维度为 0 返回 [`NameBudgetError::Zero`]；超过跨端上限返回
    /// [`NameBudgetError::ExceedsCrossPlatform`]。
    pub fn new(max_utf8_bytes: usize, max_utf16_units: usize) -> Result<Self, NameBudgetError> {
        if max_utf8_bytes == 0 || max_utf16_units == 0 {
            return Err(NameBudgetError::Zero);
        }
        if max_utf8_bytes > MAX_COMPONENT_UTF8_BYTES || max_utf16_units > MAX_COMPONENT_UTF16_UNITS
        {
            return Err(NameBudgetError::ExceedsCrossPlatform {
                utf8_bytes: max_utf8_bytes,
                utf16_units: max_utf16_units,
            });
        }
        Ok(Self {
            max_utf8_bytes,
            max_utf16_units,
        })
    }

    /// 允许的最大 UTF-8 字节数。
    #[must_use]
    pub const fn max_utf8_bytes(self) -> usize {
        self.max_utf8_bytes
    }

    /// 允许的最大 UTF-16 码元数。
    #[must_use]
    pub const fn max_utf16_units(self) -> usize {
        self.max_utf16_units
    }
}

/// 名称预算不合规的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameBudgetError {
    /// 任一维度为 0：任何名称都放不下。
    Zero,
    /// 超出跨端可表示性的上限，两端不可能无损表示该名称。
    ExceedsCrossPlatform {
        /// 请求的字节上限。
        utf8_bytes: usize,
        /// 请求的码元上限。
        utf16_units: usize,
    },
}

impl fmt::Display for NameBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => f.write_str("名称预算不能为 0"),
            Self::ExceedsCrossPlatform {
                utf8_bytes,
                utf16_units,
            } => write!(
                f,
                "名称预算 {utf8_bytes} 字节 / {utf16_units} 码元超过跨端上限 {MAX_COMPONENT_UTF8_BYTES} / {MAX_COMPONENT_UTF16_UNITS}"
            ),
        }
    }
}

impl core::error::Error for NameBudgetError {}

/// 冲突名称的组成输入（design §10.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictNameRequest<'a> {
    /// 设备短标识：只作名称标签，不承担身份认证（design §7.1）；不得含路径/Windows 非法字符。
    pub device_label: &'a str,
    /// 触发该冲突的逻辑操作身份；名称里取它的前 [`ShortIdLen`] 个十六进制字符。
    pub op_id: OpId,
    /// 服务器为该提交记录的时间；名称里的时间必须来自服务器，不用客户端时钟（design §9.1）。
    pub server_time: UtcTimestamp,
    /// 目标文件系统的名称预算。
    pub budget: NameBudget,
    /// 名称中短标识的长度。
    pub short_id_len: ShortIdLen,
}

/// 生成冲突名称失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictNameError {
    /// 输入的相对路径没有名称分量（[`RelativePath`] 保证至少一个分量；本变体只防御破坏的输入）。
    NameMissing,
    /// 后缀与扩展名本身已超出预算：压缩原名干也无法满足，且不能用丢身份信息的方式硬塞。
    SuffixExceedsBudget {
        /// 后缀与扩展名合计需要的 UTF-8 字节数。
        required_utf8_bytes: usize,
        /// 后缀与扩展名合计需要的 UTF-16 码元数。
        required_utf16_units: usize,
    },
    /// 组合出的名称不可跨端表示（例如设备短标识含 Windows 非法字符、保留设备名或过长）。
    Unrepresentable(NameIssue),
    /// 组合出的相对路径超长（[`RelativePath`] 的整体上限）。
    Path(PathIssue),
}

impl fmt::Display for ConflictNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameMissing => f.write_str("输入路径没有名称分量，无法生成冲突名称"),
            Self::SuffixExceedsBudget {
                required_utf8_bytes,
                required_utf16_units,
            } => write!(
                f,
                "冲突后缀与扩展名需要 {required_utf8_bytes} 字节 / {required_utf16_units} 码元，超出名称预算"
            ),
            Self::Unrepresentable(issue) => write!(f, "冲突名称不可跨端表示：{issue}"),
            Self::Path(issue) => write!(f, "冲突条目路径不合规：{issue}"),
        }
    }
}

impl core::error::Error for ConflictNameError {}

/// 按 design §10.2 在**同一父目录**下生成冲突条目路径。
///
/// 名称形式为 `<原名干>（冲突-<设备短标识>-<服务器时间>-<op 短标识>）<扩展名>`：扩展名保留，
/// 时间取 `request.server_time`，原名干保留尽可能长的前缀以塞进 `request.budget`。原名干按
/// 字符边界截断（不切开多字节字符，也不改写字符）。
///
/// # Errors
///
/// 见 [`ConflictNameError`]：后缀放不下、组合结果不可跨端表示（设备短标识违规）或相对路径超长。
pub fn conflict_entry_path(
    original: &RelativePath,
    request: &ConflictNameRequest<'_>,
) -> Result<RelativePath, ConflictNameError> {
    let components = original.components();
    let Some((name, parent)) = components.split_last() else {
        return Err(ConflictNameError::NameMissing);
    };
    let (stem, extension) = split_extension(name);

    let op_hex = request.op_id.to_hex();
    let suffix = format!(
        "（冲突-{}-{}-{}）",
        request.device_label,
        compact_utc(request.server_time),
        &op_hex[..request.short_id_len.get()]
    );
    let tail = format!("{suffix}{extension}");
    let tail_utf8_bytes = tail.len();
    let tail_utf16_units = tail.encode_utf16().count();
    let budget = request.budget;
    if tail_utf8_bytes > budget.max_utf8_bytes() || tail_utf16_units > budget.max_utf16_units() {
        return Err(ConflictNameError::SuffixExceedsBudget {
            required_utf8_bytes: tail_utf8_bytes,
            required_utf16_units: tail_utf16_units,
        });
    }

    let room_utf8_bytes = budget.max_utf8_bytes() - tail_utf8_bytes;
    let room_utf16_units = budget.max_utf16_units() - tail_utf16_units;
    let mut kept_bytes = 0;
    let mut kept_units = 0;
    let mut cut = 0;
    for character in stem.chars() {
        let byte_len = character.len_utf8();
        let unit_len = character.len_utf16();
        if kept_bytes + byte_len > room_utf8_bytes || kept_units + unit_len > room_utf16_units {
            break;
        }
        kept_bytes += byte_len;
        kept_units += unit_len;
        cut += byte_len;
    }
    let assembled = format!("{}{tail}", &stem[..cut]);
    validate_name(&assembled).map_err(ConflictNameError::Unrepresentable)?;

    let mut text = parent.join("/");
    if !text.is_empty() {
        text.push('/');
    }
    text.push_str(&assembled);
    RelativePath::parse(&text).map_err(ConflictNameError::Path)
}

/// 把名称拆成“原干”与“扩展名（含 `.`）”。
///
/// 扩展名取最后一个 `.` 之后的部分；首个字符就是 `.` 的名称（`.gitignore`）整段都算原干，
/// 与 Windows 把这类名称当完整名称处理一致；没有 `.` 时扩展名为空。
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => name.split_at(index),
        _ => (name, ""),
    }
}

/// 名称里的 UTC 紧凑形式 `20261105T083000Z`（design §10.2）。
fn compact_utc(timestamp: UtcTimestamp) -> String {
    let seconds = timestamp.as_unix_seconds();
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

/// 由 1970-01-01 起的天数换算公历年月日（Howard Hinnant 的 `civil_from_days`：纯整数运算、
/// 对 1970 之前与世纪闰年规则都成立，因此不引入日历库也不读时钟）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * march_month + 2) / 5 + 1) as u32;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let month = month as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 服务端幂等记录里该 `OpId` 的既有冲突结果（design §10.1 末行、T03）。
///
/// 服务端在同一事务里提交“冲突条目”和“该 `OpId` 的结果记录”（design I01），因此重试时这条记录
/// 存在就说明冲突副本已经提交：必须返回同一身份与同一路径，不能再造第二份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedConflict {
    /// 记录所属的逻辑操作身份。
    pub op_id: OpId,
    /// 已提交的冲突条目身份。
    pub entry_id: EntryId,
    /// 首次提交时生成的冲突条目路径（重试不得改名，否则会产生第二份）。
    pub path: RelativePath,
    /// 已提交的迟到分支内容。
    pub content: ContentFingerprint,
}

/// 迟到分支内容的冲突条目计划。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictCopy {
    /// 首次提交：在冲突路径按该身份创建新条目。
    Create(Plan),
    /// 同一 `OpId` 已有结果：返回原记录，不再创建第二份（design §10.1 末行、T03）。
    Reuse(CommittedConflict),
}

/// 内容冲突的完整计划（design §10.1“两端修改内容且字节不同”）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictPlan {
    /// 主条目：服务器已提交 head 保留原路径。替换本地字节前必须先提交冲突副本
    /// （依赖里的 [`Dependency::ConflictCopyCommitted`]）。
    pub main: Plan,
    /// 迟到分支内容：新冲突条目，或同一 `OpId` 的既有结果。
    pub copy: ConflictCopy,
}

/// 规划内容冲突的输入。
#[derive(Clone, Copy, Debug)]
pub struct ContentConflictRequest<'a> {
    /// 共同基线 B。
    pub base: &'a ServerFact,
    /// 迟到分支的本地事实 L。
    pub local: &'a LocalFact,
    /// 服务器已提交的 head R。
    pub remote: &'a ServerFact,
    /// 服务端为本次操作首次分配并持久化的冲突条目身份；重试必须沿用同一值（I06）。
    pub conflict_entry_id: EntryId,
    /// 同一 `OpId` 已记录的提交结果；重试时传入，首次提交为 `None`。
    pub committed: Option<&'a CommittedConflict>,
    /// 冲突名称的组成输入（含 `OpId`、设备短标识、服务器时间、预算）。
    pub naming: ConflictNameRequest<'a>,
}

/// 内容冲突规划被拒绝的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictPlanError {
    /// 输入不是“两端都改内容且字节不同”的冲突：应走 `reconcile` 的其它分支（无操作、应用远端、
    /// 本地提交、同结果归并、延期裁决）。
    NotContentConflict,
    /// 服务器 head 自称文件却没有内容版本，无法给出保留主条目的计划（事实不完整）。
    IncompleteHead,
    /// 同一 `OpId` 已记录的结果与本次请求不一致（冲突身份、内容或操作身份不同）：按 I06 拒绝，
    /// 不把它当成新请求，也不返回另一份内容的结果。
    OpIdRequestChanged,
    /// 冲突名称无法在预算内生成。
    Name(ConflictNameError),
}

impl fmt::Display for ConflictPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotContentConflict => f.write_str("输入不是两端内容不同冲突，不生成冲突副本"),
            Self::IncompleteHead => f.write_str("服务器 head 是文件但没有内容版本，无法保留主条目"),
            Self::OpIdRequestChanged => {
                f.write_str("同一 OpId 已记录的结果与本次请求不同，按幂等规则拒绝")
            }
            Self::Name(error) => write!(f, "冲突名称生成失败：{error}"),
        }
    }
}

impl core::error::Error for ConflictPlanError {}

/// 规划内容冲突：服务器 head 保留主路径，迟到分支的不同内容成为稳定身份的冲突副本。
///
/// 输入必须正好是“两端都相对基线改了内容、且两侧内容不同、位置与类型未变”的情形；其它输入
/// 返回 [`ConflictPlanError::NotContentConflict`]，由调用方交回 `reconcile` 的对应分支处理。
/// 内容相同（长度与 SHA-256 都相同）不创建副本，即使名称、路径或 `mtime` 相同也不归并出错。
///
/// # Errors
///
/// 见 [`ConflictPlanError`]：不是本卡的冲突形态、head 缺少内容版本、同一 `OpId` 的请求不一致，
/// 或冲突名称无法在预算内生成。
pub fn plan_content_conflict(
    request: &ContentConflictRequest<'_>,
) -> Result<ConflictPlan, ConflictPlanError> {
    let ContentConflictRequest {
        base,
        local,
        remote,
        conflict_entry_id,
        committed,
        naming,
    } = *request;

    let (
        EntryKind::File(base_content),
        EntryKind::File(local_content),
        EntryKind::File(remote_content),
    ) = (base.kind, local.kind, remote.kind)
    else {
        return Err(ConflictPlanError::NotContentConflict);
    };
    if local.path != remote.path || remote.path != base.path {
        return Err(ConflictPlanError::NotContentConflict);
    }
    if remote.entry_id != base.entry_id {
        return Err(ConflictPlanError::NotContentConflict);
    }
    // 同一结果（长度与摘要都相同）走归并；只有两侧都改且互不相同才是内容冲突。
    if base_content == local_content
        || base_content == remote_content
        || local_content == remote_content
    {
        return Err(ConflictPlanError::NotContentConflict);
    }

    if let Some(record) = committed {
        if record.op_id != naming.op_id
            || record.entry_id != conflict_entry_id
            || record.content != local_content
        {
            return Err(ConflictPlanError::OpIdRequestChanged);
        }
    }

    let main = retain_server_head(base, local, remote, conflict_entry_id)?;
    let copy = match committed {
        Some(record) => ConflictCopy::Reuse(record.clone()),
        None => {
            let path =
                conflict_entry_path(&local.path, &naming).map_err(ConflictPlanError::Name)?;
            ConflictCopy::Create(create_copy_plan(
                path,
                remote,
                local,
                conflict_entry_id,
                naming.op_id,
            ))
        }
    };
    Ok(ConflictPlan { main, copy })
}

/// 主条目保留服务器 head，并等待冲突副本提交后再替换本地字节（I02）。
fn retain_server_head(
    base: &ServerFact,
    local: &LocalFact,
    remote: &ServerFact,
    conflict_entry_id: EntryId,
) -> Result<Plan, ConflictPlanError> {
    let mut plan = apply_remote(remote.path.clone(), base, local, remote);
    if !matches!(plan.action, Action::ApplyRemote { .. }) {
        return Err(ConflictPlanError::IncompleteHead);
    }
    plan.dependencies.push(Dependency::ConflictCopyCommitted {
        entry_id: conflict_entry_id,
    });
    Ok(plan)
}

/// 迟到分支内容的新条目计划：上传该代际的本地快照，不覆盖主路径。
fn create_copy_plan(
    path: RelativePath,
    remote: &ServerFact,
    local: &LocalFact,
    entry_id: EntryId,
    op_id: OpId,
) -> Plan {
    Plan {
        path,
        action: Action::CreateConflictCopy {
            entry_id,
            op_id,
            generation: local.generation,
        },
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: remote.revision,
            },
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
            Prerequisite::TargetPathUsable,
        ],
        dependencies: vec![
            Dependency::ParentDirectoryReady,
            Dependency::SnapshotLocalContent {
                generation: local.generation,
            },
        ],
        local_protection: LocalProtection::UploadSnapshot {
            generation: local.generation,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommittedConflict, ConflictCopy, ConflictNameError, ConflictNameRequest, ConflictPlanError,
        ContentConflictRequest, NameBudget, NameBudgetError, ShortIdLen, ShortIdLenError,
        civil_from_days, compact_utc, conflict_entry_path, plan_content_conflict,
    };
    use crate::reconcile::{
        Action, ContentFingerprint, Dependency, EntryKind, LocalFact, LocalProtection,
        Prerequisite, ServerFact, reconcile,
    };
    use vaultsyncd_domain::ids::{EntryId, OpId, VersionId};
    use vaultsyncd_domain::version::{Generation, Revision, UtcTimestamp};
    use vaultsyncd_fs_common::name::NameIssue;
    use vaultsyncd_fs_common::path::RelativePath;

    const ENTRY: EntryId = EntryId::from_bytes([0x11; 16]);
    const OTHER_ENTRY: EntryId = EntryId::from_bytes([0x22; 16]);
    const COPY_ENTRY: EntryId = EntryId::from_bytes([0xc0; 16]);
    const OTHER_COPY_ENTRY: EntryId = EntryId::from_bytes([0xc1; 16]);
    const VERSION_OLD: VersionId = VersionId::from_bytes([0xa1; 16]);
    const VERSION_NEW: VersionId = VersionId::from_bytes([0xb2; 16]);
    const OP: OpId = OpId::from_bytes([0x5a; 16]);
    const OTHER_OP: OpId = OpId::from_bytes([0x5b; 16]);

    /// 2026-11-05T08:30:00Z，design §10.2 示例里的服务器时间形态。
    const SERVER_TIME: i64 = 1_793_867_400;
    const DEVICE_LABEL: &str = "3f7c9a01";
    /// 名称里 8 位 op 短标识（`OP` 的规范文本前 8 个字符）。
    const OP_SHORT: &str = "5a5a5a5a";
    const EXPECTED_NAME: &str = "report（冲突-3f7c9a01-20261105T083000Z-5a5a5a5a）.txt";

    fn path(text: &str) -> RelativePath {
        RelativePath::parse(text).expect("测试路径合法")
    }

    fn content(digest: u8, size: u64) -> ContentFingerprint {
        ContentFingerprint::new(size, [digest; 32])
    }

    fn revision(value: u64) -> Revision {
        Revision::new(value).expect("测试修订在范围内")
    }

    fn generation(value: u64) -> Generation {
        Generation::new(value).expect("测试代际在范围内")
    }

    fn mtime(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("测试时间在范围内")
    }

    fn server_file(digest: u8, revision_value: u64, version: VersionId) -> ServerFact {
        ServerFact::file(
            path("docs/report.txt"),
            content(digest, 120),
            ENTRY,
            revision(revision_value),
            version,
        )
    }

    fn local_file(digest: u8, generation_value: u64) -> LocalFact {
        LocalFact::file(
            path("docs/report.txt"),
            content(digest, 120),
            generation(generation_value),
        )
    }

    fn naming() -> ConflictNameRequest<'static> {
        ConflictNameRequest {
            device_label: DEVICE_LABEL,
            op_id: OP,
            server_time: mtime(SERVER_TIME),
            budget: NameBudget::CROSS_PLATFORM,
            short_id_len: ShortIdLen::DEFAULT,
        }
    }

    /// 三方输入与默认名称输入组成一次请求。
    fn request<'a>(
        base: &'a ServerFact,
        local: &'a LocalFact,
        remote: &'a ServerFact,
        naming: ConflictNameRequest<'a>,
    ) -> ContentConflictRequest<'a> {
        ContentConflictRequest {
            base,
            local,
            remote,
            conflict_entry_id: COPY_ENTRY,
            committed: None,
            naming,
        }
    }

    #[test]
    fn retains_committed_head_and_creates_late_branch_entry() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xcc, 9);
        let remote = server_file(0xbb, 5, VERSION_NEW);

        let plan =
            plan_content_conflict(&request(&base, &local, &remote, naming())).expect("可规划");

        assert_eq!(plan.main.path, path("docs/report.txt"), "主条目留在原路径");
        assert_eq!(
            plan.main.action,
            Action::ApplyRemote {
                revision: revision(5),
                version: Some(VERSION_NEW),
            }
        );
        assert_eq!(
            plan.main.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(9)
                },
                Prerequisite::UsagePermitsChange,
                Prerequisite::TargetPathUsable,
            ]
        );
        assert_eq!(
            plan.main.dependencies,
            [
                Dependency::DownloadVersion {
                    version: VERSION_NEW
                },
                Dependency::ConflictCopyCommitted {
                    entry_id: COPY_ENTRY
                },
            ],
            "替换本地字节前必须先提交冲突副本"
        );
        assert_eq!(
            plan.main.local_protection,
            LocalProtection::SnapshotBeforeReplace {
                generation: generation(9)
            }
        );

        let ConflictCopy::Create(copy) = plan.copy else {
            panic!("首次提交必须创建冲突条目");
        };
        assert_eq!(copy.path, path(&format!("docs/{EXPECTED_NAME}")));
        assert_eq!(
            copy.action,
            Action::CreateConflictCopy {
                entry_id: COPY_ENTRY,
                op_id: OP,
                generation: generation(9),
            }
        );
        assert_eq!(
            copy.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(5)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(9)
                },
                Prerequisite::TargetPathUsable,
            ]
        );
        assert_eq!(
            copy.dependencies,
            [
                Dependency::ParentDirectoryReady,
                Dependency::SnapshotLocalContent {
                    generation: generation(9)
                },
            ]
        );
        assert_eq!(
            copy.local_protection,
            LocalProtection::UploadSnapshot {
                generation: generation(9)
            }
        );
    }

    #[test]
    fn same_content_merges_and_never_becomes_a_copy() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        // 内容相同、mtime 相差极大：仍然是同一结果。
        let local = local_file(0xdd, 9).with_observed_mtime(mtime(2_000_000_000));
        let remote = server_file(0xdd, 6, VERSION_NEW).with_observed_mtime(mtime(0));

        let reconcile_plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");
        assert_eq!(
            reconcile_plan.action,
            Action::MergeSameResult {
                revision: revision(6),
                version: Some(VERSION_NEW),
            }
        );
        assert_eq!(
            plan_content_conflict(&request(&base, &local, &remote, naming())).unwrap_err(),
            ConflictPlanError::NotContentConflict,
            "同一内容应归并不造副本"
        );
    }

    #[test]
    fn content_comparison_uses_length_and_digest_together() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let remote = server_file(0xbb, 5, VERSION_NEW);

        // 长度相同、摘要不同：真冲突。
        let same_length = local_file(0xcc, 9);
        assert_eq!(same_length.kind, EntryKind::File(content(0xcc, 120)));
        assert_eq!(remote.kind, EntryKind::File(content(0xbb, 120)));
        assert!(matches!(
            plan_content_conflict(&request(&base, &same_length, &remote, naming()))
                .expect("长度相同摘要不同是冲突")
                .copy,
            ConflictCopy::Create(_)
        ));

        // 摘要相同、长度不同：也不是同一内容。
        let same_digest =
            LocalFact::file(path("docs/report.txt"), content(0xdd, 121), generation(9));
        let remote_same_digest = ServerFact::file(
            path("docs/report.txt"),
            content(0xdd, 120),
            ENTRY,
            revision(5),
            VERSION_NEW,
        );
        assert!(matches!(
            plan_content_conflict(&request(
                &server_file(0xaa, 4, VERSION_OLD),
                &same_digest,
                &remote_same_digest,
                naming()
            ))
            .expect("长度不同是冲突")
            .copy,
            ConflictCopy::Create(_)
        ));
    }

    #[test]
    fn repeated_planning_returns_the_same_copy() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xcc, 9);
        let remote = server_file(0xbb, 5, VERSION_NEW);

        let first =
            plan_content_conflict(&request(&base, &local, &remote, naming())).expect("可规划");
        let repeated =
            plan_content_conflict(&request(&base, &local, &remote, naming())).expect("可规划");

        assert_eq!(first, repeated, "同一 OpId 与同一输入必须得到同一份计划");
        let (ConflictCopy::Create(first_copy), ConflictCopy::Create(repeated_copy)) =
            (&first.copy, &repeated.copy)
        else {
            panic!("两次都应是创建计划");
        };
        assert_eq!(first_copy.path, repeated_copy.path);
    }

    #[test]
    fn committed_op_reuses_the_original_copy() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xcc, 9);
        let remote = server_file(0xbb, 5, VERSION_NEW);
        let committed = CommittedConflict {
            op_id: OP,
            entry_id: COPY_ENTRY,
            path: path(&format!("docs/{EXPECTED_NAME}")),
            content: content(0xcc, 120),
        };

        // 重试时设备短标识与服务器时间都可能变，但已提交结果必须原样返回。
        let mut retry_naming = naming();
        retry_naming.device_label = "ffffffff";
        retry_naming.server_time = mtime(SERVER_TIME + 86_400);
        let mut retry = request(&base, &local, &remote, retry_naming);
        retry.committed = Some(&committed);

        let plan = plan_content_conflict(&retry).expect("重试可规划");

        assert_eq!(plan.copy, ConflictCopy::Reuse(committed.clone()));
        assert_eq!(
            plan.main,
            plan_content_conflict(&request(&base, &local, &remote, naming()))
                .expect("首次可规划")
                .main,
            "重试不改变主条目计划"
        );
    }

    #[test]
    fn committed_op_with_a_different_request_is_rejected() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xcc, 9);
        let remote = server_file(0xbb, 5, VERSION_NEW);
        let committed = CommittedConflict {
            op_id: OP,
            entry_id: COPY_ENTRY,
            path: path(&format!("docs/{EXPECTED_NAME}")),
            content: content(0xcc, 120),
        };

        // 内容不同：同 ID 不同请求。
        let other_content = CommittedConflict {
            content: content(0xee, 120),
            ..committed.clone()
        };
        let mut with_other_content = request(&base, &local, &remote, naming());
        with_other_content.committed = Some(&other_content);
        assert_eq!(
            plan_content_conflict(&with_other_content).unwrap_err(),
            ConflictPlanError::OpIdRequestChanged
        );

        // 冲突身份不同：不能把另一条冲突的结果当成这次结果。
        let mut with_other_entry = request(&base, &local, &remote, naming());
        with_other_entry.committed = Some(&committed);
        with_other_entry.conflict_entry_id = OTHER_COPY_ENTRY;
        assert_eq!(
            plan_content_conflict(&with_other_entry).unwrap_err(),
            ConflictPlanError::OpIdRequestChanged
        );

        // 记录属于另一个 OpId：不能串用。
        let foreign = CommittedConflict {
            op_id: OTHER_OP,
            ..committed.clone()
        };
        let mut foreign_request = request(&base, &local, &remote, naming());
        foreign_request.committed = Some(&foreign);
        assert_eq!(
            plan_content_conflict(&foreign_request).unwrap_err(),
            ConflictPlanError::OpIdRequestChanged
        );
    }

    #[test]
    fn swapping_device_clocks_does_not_change_the_version_decision() {
        let base = server_file(0xaa, 4, VERSION_OLD).with_observed_mtime(mtime(100));
        let remote_early = server_file(0xbb, 5, VERSION_NEW).with_observed_mtime(mtime(200));
        let remote_late =
            server_file(0xbb, 5, VERSION_NEW).with_observed_mtime(mtime(2_000_000_000));

        // 第一种：本地时钟慢；第二种：本地时钟快（mtime 在服务器之后）。
        let local_slow = local_file(0xcc, 9).with_observed_mtime(mtime(0));
        let local_fast = local_file(0xcc, 9).with_observed_mtime(mtime(2_000_000_001));
        assert_ne!(local_slow, local_fast);

        let first = plan_content_conflict(&request(&base, &local_slow, &remote_early, naming()))
            .expect("可规划");
        let second = plan_content_conflict(&request(&base, &local_fast, &remote_late, naming()))
            .expect("可规划");

        assert_eq!(first, second, "胜者只看服务器提交序，不看任何 mtime");
        assert_eq!(
            first.main.action,
            Action::ApplyRemote {
                revision: revision(5),
                version: Some(VERSION_NEW),
            },
            "服务器已提交 head 保留主路径"
        );
    }

    #[test]
    fn other_conflict_shapes_are_left_to_their_own_rules() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let remote = server_file(0xbb, 5, VERSION_NEW);

        // 只有远端变：本地仍等于基线。
        let unchanged_local = local_file(0xaa, 7);
        // 只有本地变：远端仍等于基线内容。
        let unchanged_remote = server_file(0xaa, 4, VERSION_OLD);
        let changed_local = local_file(0xcc, 9);
        // 位置不同（一端移动）与身份被替换都属移动/删除矩阵。
        let moved_local =
            LocalFact::file(path("docs/renamed.txt"), content(0xcc, 120), generation(9));
        let replaced_remote = ServerFact::file(
            path("docs/report.txt"),
            content(0xbb, 120),
            OTHER_ENTRY,
            revision(5),
            VERSION_NEW,
        );
        // 类型不同（文件与目录同名）。
        let directory_local = LocalFact::directory(path("docs/report.txt"), generation(9));

        let cases = [
            ("只有远端改内容", &unchanged_local, &remote),
            ("只有本地改内容", &changed_local, &unchanged_remote),
            ("一端移动", &moved_local, &remote),
            ("远端身份已替换", &changed_local, &replaced_remote),
            ("本地是目录", &directory_local, &remote),
        ];
        for (name, local, remote_fact) in cases {
            assert_eq!(
                plan_content_conflict(&request(&base, local, remote_fact, naming())).unwrap_err(),
                ConflictPlanError::NotContentConflict,
                "{name}"
            );
        }
    }

    #[test]
    fn head_without_a_content_version_is_reported() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xcc, 9);
        let remote = ServerFact {
            path: path("docs/report.txt"),
            kind: EntryKind::File(content(0xbb, 120)),
            entry_id: ENTRY,
            revision: revision(5),
            version: None,
            observed_mtime: None,
        };

        assert_eq!(
            plan_content_conflict(&request(&base, &local, &remote, naming())).unwrap_err(),
            ConflictPlanError::IncompleteHead
        );
    }

    #[test]
    fn conflict_name_keeps_the_extension_within_the_budget() {
        let original = path(&format!("docs/{}.docx", "报".repeat(80)));
        let budget = NameBudget::new(120, 120).expect("预算有效");
        let mut request = naming();
        request.budget = budget;

        let generated = conflict_entry_path(&original, &request).expect("可生成");

        let components = generated.components();
        assert_eq!(components.len(), 2, "冲突条目与原名同父目录");
        assert_eq!(components[0], "docs");
        let name = &components[1];
        assert!(name.len() <= budget.max_utf8_bytes());
        assert!(
            name.encode_utf16().count() <= budget.max_utf16_units(),
            "码元预算也要满足"
        );
        // 后缀 52 字节 / 44 码元，余量 68 字节 / 76 码元 → 保留 22 个汉字（66 字节）。
        assert_eq!(
            name.as_str(),
            format!(
                "{}（冲突-3f7c9a01-20261105T083000Z-{OP_SHORT}）.docx",
                "报".repeat(22)
            )
        );
        assert!(name.ends_with(".docx"), "扩展名保留");
    }

    #[test]
    fn conflict_name_keeps_short_names_untouched() {
        let original = path("docs/report.txt");

        let generated = conflict_entry_path(&original, &naming()).expect("可生成");

        assert_eq!(generated, path(&format!("docs/{EXPECTED_NAME}")));
    }

    #[test]
    fn short_identifiers_extend_on_name_collisions() {
        let original = path("docs/report.txt");
        let default_len = ShortIdLen::DEFAULT;
        let extended_len = default_len.extended().expect("还可加长");
        assert_eq!(extended_len, ShortIdLen::new(16).expect("16 合法"));
        assert_eq!(extended_len.get(), 16);

        let mut wider = naming();
        wider.short_id_len = extended_len;
        let default_name = conflict_entry_path(&original, &naming()).expect("可生成");
        let wider_name = conflict_entry_path(&original, &wider).expect("可生成");

        assert_ne!(default_name, wider_name, "撞名时靠加长短标识换名");
        assert!(
            wider_name
                .components()
                .last()
                .expect("有名称")
                .contains("（冲突-3f7c9a01-20261105T083000Z-5a5a5a5a5a5a5a5a）")
        );
        // 名字不同但 OpId 相同：文件的身份仍由服务端分配的冲突身份决定。
        assert_eq!(
            ShortIdLen::new(32).expect("用满整个规范文本").extended(),
            None
        );
        assert_eq!(
            ShortIdLen::new(7).unwrap_err(),
            ShortIdLenError::TooShort { len: 7, min: 8 }
        );
        assert_eq!(
            ShortIdLen::new(33).unwrap_err(),
            ShortIdLenError::TooLong { len: 33, max: 32 }
        );
    }

    #[test]
    fn impossible_budgets_are_reported() {
        let original = path("docs/report.txt");
        let mut tiny = naming();
        tiny.budget = NameBudget::new(20, 20).expect("预算有效");
        assert_eq!(
            conflict_entry_path(&original, &tiny).unwrap_err(),
            ConflictNameError::SuffixExceedsBudget {
                required_utf8_bytes: 51,
                required_utf16_units: 43,
            }
        );

        assert_eq!(NameBudget::new(0, 255).unwrap_err(), NameBudgetError::Zero);
        assert_eq!(
            NameBudget::new(256, 255).unwrap_err(),
            NameBudgetError::ExceedsCrossPlatform {
                utf8_bytes: 256,
                utf16_units: 255,
            }
        );
        assert_eq!(NameBudget::CROSS_PLATFORM.max_utf8_bytes(), 255);
        assert_eq!(NameBudget::CROSS_PLATFORM.max_utf16_units(), 255);
    }

    #[test]
    fn device_label_with_an_illegal_character_is_unrepresentable() {
        let original = path("docs/report.txt");
        let mut request = naming();
        request.device_label = "a:b";

        assert_eq!(
            conflict_entry_path(&original, &request).unwrap_err(),
            ConflictNameError::Unrepresentable(NameIssue::Illegal(':'))
        );
    }

    #[test]
    fn timestamps_use_the_server_time_in_compact_utc() {
        assert_eq!(compact_utc(mtime(0)), "19700101T000000Z");
        assert_eq!(compact_utc(mtime(SERVER_TIME)), "20261105T083000Z");
        assert_eq!(compact_utc(mtime(1_709_251_199)), "20240229T235959Z");
        assert_eq!(compact_utc(mtime(951_825_600)), "20000229T120000Z");
        assert_eq!(compact_utc(mtime(4_107_542_400)), "21000301T000000Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
