//! 释放（dehydrate）与写入保护判定（S012）。
//!
//! 顺序与不变量（design §8.4、§14.4；T07/T27/T28）：
//!
//! 1. **先观察、不改状态**：`CfGetPlaceholderInfo` 给出 `OnDiskDataSize`/`InSyncState`/
//!    `PinState`/`FileIdentity`。已脱水（`OnDiskDataSize=0`）直接返回，不探测也不读内容，
//!    避免“为了判断而先水合”。
//! 2. **使用保护**：三个共享模式探测（`GENERIC_READ`+不共享、`GENERIC_WRITE`+不共享、
//!    `GENERIC_READ`+全共享）。只开句柄、立即关闭，不改内容。
//! 3. **已观察 generation**：读本地内容算 SHA-256，与固定内容版本比较；再读一遍确认两次
//!    哈希一致（观察窗口内内容没有变化）。
//! 4. **只有 [`Verdict::Clean`] 才释放**：先 `CfSetInSyncState(IN_SYNC)` 声明“本地内容等于
//!    已确认版本”（这是 `CfDehydratePlaceholder` 的前置条件），再
//!    `CfDehydratePlaceholder(0, 逻辑大小)`。绝不手改 reparse point、不截断文件伪造脱水。
//! 5. **释放后复核**：重新读 `OnDiskDataSize`，只有它为 0 才报告 `released`；平台拒绝时保留
//!    原始 `HRESULT` 并报告 `dehydrate_failed`，不把失败当成功。
//!
//! 判定规则本身在 [`super::guard`]（纯逻辑、宿主可测）；本模块只负责取证据与调用平台。

pub mod verified;

use std::path::{Path, PathBuf};

use super::content;
use super::guard::{self, ContentCheck, Observation, ProbeOutcome, UsageProbe, Verdict};
use super::identity::FileIdentity;
use super::lifecycle::{HResult, StageError, record};
use super::prototype::{self, EntryObservation, hresult_from_io, hresult_from_win32, wide};
use super::provider::{ProviderConfig, ProviderSession, entry_path, require_entry};
use super::raw::*;

/// `release` 的实际结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// 判定 `Clean` 且平台释放成功、本地数据已为 0。
    Released,
    /// 进入时已经脱水：没有本地内容可丢，按“已释放”处理。
    AlreadyDehydrated,
    /// 判定不是 `Clean`，因此**没有**调用平台释放。
    Refused,
    /// 判定 `Clean` 但平台释放失败（`HRESULT` 见报告）；本地内容保持不变。
    DehydrateFailed,
}

impl ReleaseOutcome {
    /// 日志/命令行里的名字；客体脚本按这些值断言。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::AlreadyDehydrated => "already_dehydrated",
            Self::Refused => "refused",
            Self::DehydrateFailed => "dehydrate_failed",
        }
    }

    /// 内容是否已经不在本地（`released`/`already_dehydrated`）。
    pub const fn is_released(self) -> bool {
        matches!(self, Self::Released | Self::AlreadyDehydrated)
    }
}

/// 判定阶段收集到的全部证据（`protect` 的输出就是它）。
#[derive(Clone, Debug)]
pub struct GuardReport {
    /// 条目名。
    pub name: String,
    /// 判定。
    pub verdict: Verdict,
    /// 判定理由。
    pub reason: &'static str,
    /// 内容哈希之后的系统状态。
    pub observation: Observation,
    /// 使用保护探测。
    pub usage: UsageProbe,
    /// 内容核对结论。
    pub content: ContentCheck,
    /// 两次内容哈希是否一致。
    pub content_stable: bool,
    /// 本地内容 SHA-256（读不到时为空）。
    pub local_sha256: String,
    /// 固定内容版本的期望 SHA-256。
    pub expected_sha256: String,
    /// `FileIdentity` 里的版本提示（解码失败为 `None`）。
    pub version_hint: Option<u64>,
}

impl GuardReport {
    /// 判定相关日志字段。
    fn fields(&self) -> Vec<(&'static str, String)> {
        let observation = &self.observation;
        let mut fields = vec![
            ("name", self.name.clone()),
            ("verdict", self.verdict.name().to_string()),
            ("reason", self.reason.to_string()),
            ("placeholder", observation.is_placeholder.to_string()),
            ("pin_state", observation.pin_state.to_string()),
            ("in_sync_state", observation.in_sync_state.to_string()),
            (
                "on_disk_data_size",
                observation.on_disk_data_size.to_string(),
            ),
            ("logical_size", observation.logical_size.to_string()),
            (
                "modified_data_size",
                observation.modified_data_size.to_string(),
            ),
            ("content", self.content.name().to_string()),
            ("content_stable", self.content_stable.to_string()),
            ("local_sha256", dash_if_empty(&self.local_sha256)),
            ("expected_sha256", dash_if_empty(&self.expected_sha256)),
            (
                "version_hint",
                self.version_hint
                    .map_or_else(|| "-".to_string(), |value| value.to_string()),
            ),
        ];
        fields.extend(self.usage.fields());
        fields
    }
}

/// 一次 `release` 的完整结果。
#[derive(Clone, Debug)]
pub struct ReleaseReport {
    /// 判定证据。
    pub guard: GuardReport,
    /// 实际结果。
    pub outcome: ReleaseOutcome,
    /// `CfSetInSyncState` 的返回值（未调用为 `None`）。
    pub in_sync_hresult: Option<HResult>,
    /// `CfDehydratePlaceholder` 的返回值（未调用为 `None`）。
    pub dehydrate_hresult: Option<HResult>,
    /// 释放后的本地位数（复核失败为 -1）。
    pub on_disk_after: i64,
    /// 释放后是否已完全脱水。
    pub dehydrated_after: bool,
}

fn dash_if_empty(text: &str) -> String {
    if text.is_empty() {
        "-".to_string()
    } else {
        text.to_string()
    }
}

fn show(path: &Path) -> String {
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// 证据收集
// ---------------------------------------------------------------------------

/// 观察 + 探测 + 内容核对的结果，连同条目路径与逻辑大小。
struct Collected {
    path: PathBuf,
    size: u64,
    report: GuardReport,
}

fn observation_of(entry: &EntryObservation) -> Observation {
    Observation {
        is_placeholder: entry.is_placeholder(),
        pin_state: entry.pin_state(),
        in_sync_state: entry.in_sync_state(),
        on_disk_data_size: entry.on_disk_data_size(),
        logical_size: entry.logical_size,
        modified_data_size: entry.modified_data_size(),
    }
}

fn observe_entry(path: &Path) -> Result<EntryObservation, StageError> {
    match prototype::try_observe(path)? {
        Some(entry) => Ok(entry),
        None => Err(StageError::new(
            "guard.observe",
            hresult_from_win32(ERROR_FILE_NOT_FOUND),
            format!("条目不存在：{}", show(path)),
        )),
    }
}

/// 收集判定证据：观察 → 使用探测 → 两次内容哈希 → 再观察。
fn collect(root: &Path, name: &str) -> Result<Collected, StageError> {
    let size = require_entry(name)?;
    let path = entry_path(root, name);
    let first = observe_entry(&path)?;
    let version_hint = FileIdentity::decode(first.identity())
        .ok()
        .map(|identity| identity.version_hint.get());
    let expected_sha256 = content::expected_digest(name).unwrap_or_default();
    let mut fields = vec![
        ("name", name.to_string()),
        ("path", show(&path)),
        ("placeholder", first.is_placeholder().to_string()),
        ("placeholder_source", first.placeholder_source().to_string()),
        (
            "placeholder_state",
            format!("0x{:08X}", first.placeholder_state),
        ),
        ("on_disk_data_size", first.on_disk_data_size().to_string()),
        ("logical_size", first.logical_size.to_string()),
        ("in_sync_state", first.in_sync_state().to_string()),
        ("pin_state", first.pin_state().to_string()),
        ("modified_data_size", first.modified_data_size().to_string()),
    ];
    fields.extend(identity_hint_field(version_hint));
    record("guard.observe", &fields, HResult::S_OK);

    if first.on_disk_data_size() <= 0 {
        // 已脱水：不探测（探测会触发水合），也不读内容。没有本地内容可丢。
        record(
            "guard.probe",
            &[
                ("name", name.to_string()),
                ("status", "skipped_dehydrated".to_string()),
            ],
            HResult::S_OK,
        );
        let report = GuardReport {
            name: name.to_string(),
            verdict: Verdict::Unknown,
            reason: "already_dehydrated",
            observation: observation_of(&first),
            usage: UsageProbe::skipped(),
            content: ContentCheck::Unverified,
            content_stable: true,
            local_sha256: String::new(),
            expected_sha256,
            version_hint,
        };
        record("guard.decision", &report.fields(), HResult::S_OK);
        return Ok(Collected { path, size, report });
    }

    let usage = probe_usage(name, &path);
    let (local_sha256, content, content_stable, hash_error) =
        check_content(name, &path, &expected_sha256);
    let second = observe_entry(&path)?;
    let observation = observation_of(&second);
    let inputs = guard::Inputs {
        observation: &observation,
        usage: &usage,
        content,
        content_stable,
    };
    let decision = guard::evaluate(&inputs);
    let report = GuardReport {
        name: name.to_string(),
        verdict: decision.verdict,
        reason: decision.reason,
        observation,
        usage,
        content,
        content_stable,
        local_sha256,
        expected_sha256,
        version_hint,
    };
    record(
        "guard.content",
        &[
            ("name", name.to_string()),
            ("path", show(&path)),
            ("local_sha256", dash_if_empty(&report.local_sha256)),
            ("expected_sha256", dash_if_empty(&report.expected_sha256)),
            ("content", content.name().to_string()),
            ("content_stable", content_stable.to_string()),
            ("error", hash_error.unwrap_or_else(|| "-".to_string())),
            (
                "on_disk_data_size",
                report.observation.on_disk_data_size.to_string(),
            ),
        ],
        HResult::S_OK,
    );
    record("guard.decision", &report.fields(), HResult::S_OK);
    Ok(Collected { path, size, report })
}

fn identity_hint_field(version_hint: Option<u64>) -> Vec<(&'static str, String)> {
    vec![(
        "version_hint",
        version_hint.map_or_else(|| "-".to_string(), |value| value.to_string()),
    )]
}

/// 三个共享模式探测；每个探测都立即关闭句柄。
fn probe_usage(name: &str, path: &Path) -> UsageProbe {
    let path_w = wide(path);
    let usage = UsageProbe {
        exclusive_read: probe_open(&path_w, GENERIC_READ, 0),
        exclusive_write: probe_open(&path_w, GENERIC_WRITE, 0),
        shared_read: probe_open(&path_w, GENERIC_READ, FILE_SHARE_ALL),
    };
    let mut fields = vec![("name", name.to_string())];
    fields.extend(usage.fields());
    record("guard.probe", &fields, HResult::S_OK);
    usage
}

fn probe_open(path_w: &[u16], desired_access: u32, share_mode: u32) -> ProbeOutcome {
    let handle = unsafe { open_with_share(path_w, desired_access, share_mode) };
    if handle == INVALID_HANDLE_VALUE {
        return ProbeOutcome::Failed(unsafe { GetLastError() });
    }
    unsafe { close_handle(handle) };
    ProbeOutcome::Opened
}

/// 读两遍内容：第一遍与期望哈希比较，两遍一致才算“观察窗口内没有变化”。
fn check_content(
    name: &str,
    path: &Path,
    expected_sha256: &str,
) -> (String, ContentCheck, bool, Option<String>) {
    let first = hash_file(path);
    let second = hash_file(path);
    match (first, second) {
        (Ok(first_hash), Ok(second_hash)) => {
            let stable = first_hash == second_hash;
            let content = if expected_sha256.is_empty() {
                ContentCheck::Unverified
            } else if first_hash == expected_sha256 {
                ContentCheck::Matched
            } else {
                ContentCheck::Mismatched
            };
            (first_hash, content, stable, None)
        }
        (Err(error), _) | (_, Err(error)) => {
            let detail = format!("读取 {name} 内容失败：{error}");
            record(
                "guard.content",
                &[
                    ("name", name.to_string()),
                    ("path", show(path)),
                    ("error", detail.clone()),
                ],
                hresult_from_io(&error),
            );
            (String::new(), ContentCheck::Unverified, false, Some(detail))
        }
    }
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(content::sha256_hex(&bytes))
}

// ---------------------------------------------------------------------------
// 命令实现
// ---------------------------------------------------------------------------

/// `protect`：只做判定与证据记录，不改变任何状态（不释放、不写 in-sync）。
pub fn run_protect(root: &Path, name: &str) -> Result<GuardReport, StageError> {
    Ok(collect(root, name)?.report)
}

/// `release`：先判定，只有 `Clean` 才释放；其余一律保留本地内容。
pub fn run_release(root: &Path, name: &str) -> Result<ReleaseReport, StageError> {
    let collected = collect(root, name)?;
    let size_i = i64::try_from(collected.size).unwrap_or(i64::MAX);
    let on_disk_before = collected.report.observation.on_disk_data_size;

    if collected.report.reason == "already_dehydrated" {
        return Ok(finish(
            collected.report,
            ReleaseOutcome::AlreadyDehydrated,
            None,
            None,
            on_disk_before,
            true,
        ));
    }
    if collected.report.verdict != Verdict::Clean {
        return Ok(finish(
            collected.report,
            ReleaseOutcome::Refused,
            None,
            None,
            on_disk_before,
            false,
        ));
    }

    // 判定为 Clean：连上提供方（释放与后续水合都由平台向连接方要内容）后释放。
    let session = ProviderSession::open(root, ProviderConfig::default())?;
    let path_w = wide(&collected.path);
    let handle = unsafe { open_with_share(&path_w, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_ALL) };
    if handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        let hr = hresult_from_win32(code);
        record(
            "release.open",
            &[
                ("path", show(&collected.path)),
                ("error_code", code.to_string()),
            ],
            hr,
        );
        session.close()?;
        return Ok(finish(
            collected.report,
            ReleaseOutcome::DehydrateFailed,
            None,
            Some(hr),
            on_disk_before,
            false,
        ));
    }

    let mut usn = 0i64;
    let in_sync_hr = HResult::new(unsafe {
        CfSetInSyncState(
            handle,
            CF_IN_SYNC_STATE_IN_SYNC,
            CF_SET_IN_SYNC_FLAG_NONE,
            &mut usn,
        )
    });
    record(
        "release.in_sync",
        &[
            ("path", show(&collected.path)),
            ("in_sync_state", CF_IN_SYNC_STATE_IN_SYNC.to_string()),
            ("usn", usn.to_string()),
        ],
        in_sync_hr,
    );
    let dehydrate_hr = HResult::new(unsafe {
        CfDehydratePlaceholder(
            handle,
            0,
            size_i,
            CF_DEHYDRATE_FLAG_NONE,
            std::ptr::null_mut(),
        )
    });
    record(
        "release.dehydrate",
        &[
            ("path", show(&collected.path)),
            ("offset", "0".to_string()),
            ("length", size_i.to_string()),
            ("flags", "none".to_string()),
        ],
        dehydrate_hr,
    );
    unsafe { close_handle(handle) };
    let after = prototype::try_observe(&collected.path)?;
    let on_disk_after = after.as_ref().map_or(-1, |entry| entry.on_disk_data_size());
    let dehydrated_after = after
        .as_ref()
        .is_some_and(|entry| entry.is_fully_dehydrated());
    session.close()?;

    let outcome = if dehydrate_hr.succeeded() && on_disk_after == 0 {
        ReleaseOutcome::Released
    } else {
        ReleaseOutcome::DehydrateFailed
    };
    Ok(finish(
        collected.report,
        outcome,
        Some(in_sync_hr),
        Some(dehydrate_hr),
        on_disk_after,
        dehydrated_after,
    ))
}

fn finish(
    report: GuardReport,
    outcome: ReleaseOutcome,
    in_sync_hresult: Option<HResult>,
    dehydrate_hresult: Option<HResult>,
    on_disk_after: i64,
    dehydrated_after: bool,
) -> ReleaseReport {
    let mut fields = report.fields();
    fields.push(("outcome", outcome.name().to_string()));
    fields.push((
        "in_sync_hresult",
        in_sync_hresult.map_or_else(|| "-".to_string(), |hr| hr.to_string()),
    ));
    fields.push((
        "dehydrate_hresult",
        dehydrate_hresult.map_or_else(|| "-".to_string(), |hr| hr.to_string()),
    ));
    fields.push(("on_disk_after", on_disk_after.to_string()));
    fields.push(("dehydrated_after", dehydrated_after.to_string()));
    record("release.result", &fields, HResult::S_OK);
    ReleaseReport {
        guard: report,
        outcome,
        in_sync_hresult,
        dehydrate_hresult,
        on_disk_after,
        dehydrated_after,
    }
}
