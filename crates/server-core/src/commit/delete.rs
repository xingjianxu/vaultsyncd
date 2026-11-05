//! 可恢复删除与回收站：子树 token、冻结清单、有界批次、墓碑与单一事务公布（design §10.3、§11.2、
//! T11/T12）。
//!
//! 删除与 put/目录创建/移动共用同一套持久意图与阶段（`prepared → published → finalized`），差别在于
//! **顺序与证据**：
//!
//! 1. **先证明前提**：请求带着条目的 revision 与（目录的）服务端子树 token。核心按库里的活跃子树
//!    重新算一遍 token，不一致就说明并发新增/改名已经发生，直接拒绝（T12，`SUBTREE_CHANGED`），
//!    不动磁盘、不写意图；
//! 2. **冻结清单**：前提成立后把整棵逻辑子树冻成 [`SubtreeImage`]（目标自身 + 所有活跃后代，顺序
//!    稳定），token 与回收站清单都从这一份清单派生，不再二次读库；
//! 3. **先持久意图再删磁盘**：`prepared` 意图记录条目现场（身份、revision、token、条目数）；
//! 4. **有界批次删除**：目录按 [`MAX_DELETE_BATCH_ENTRIES`] 分批调
//!    [`RemovalPort::remove_directory_batch`]，直到报告完成；任一失败都停在 `prepared`
//!    （[`DeleteFailure::is_committed`] 恒为 `false`，design §10.3“工作目录移除失败不能声称已删除”）；
//! 5. **复核**：删除后实测路径必须不存在，还在就报 [`DeleteFailureReason::RemovalIncomplete`]；
//! 6. **单一事务公布**：`published` 之后，回收站清单、墓碑、按批次的软删除、变更事件批次与意图收尾
//!    在同一个数据库事务里提交（[`crate::ports::trash_store::DeleteStore::apply_delete`]）。逻辑删除
//!    批次因此一次完整公布，不存在“删了一半”的可见状态。
//!
//! 子树忙：删除任务执行期间，同一子树的其他写入必须得到明确忙状态。忙标记就是**未完成的删除意图**
//! 本身（持久化在 `fs_intents`，重启后仍然有效），由 [`SubtreeBusy`] 从
//! [`DeleteIntentLog::unfinished_deletes`] 构造并供其他写路径调用
//! [`SubtreeBusy::check_write`]；本模块在开始删除前也用它拒绝“子树已经在删”的重复请求。
//!
//! 恢复（[`recover_delete_root`]）：现场是“路径还在、身份与前提一致”就重做删除，路径已不存在就只补完
//! 事务，被换成别的条目就保留现场等协调，读不到就落 `needs_recovery` 停写。补事务时要求库里的子树
//! token 仍等于意图里的 token：冻结之后又冒出新的子条目属于异常，不能悄悄一起删掉。

use std::collections::BTreeMap;
use std::fmt;

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{EntryId, ID_LEN, IntentId, RootId, VersionId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};

use crate::commit::intent::{
    JsonValue, PutPhase, StateJsonError, finish_object, parse_flat_object, take_bool, take_entry,
    take_text, take_u64,
};
use crate::commit::namespace::{
    hex_decode, hex_encode, is_descendant, take_kind, take_path, validate_path,
};
use crate::commit::put::{CommitGateError, RootCommitGate};
use crate::ports::intent_log::{DeleteIntentLog, IntentLogError};
use crate::ports::namespace::{NamespaceError, NamespacePort, PathState};
use crate::ports::object_store::FileKey;
use crate::ports::put_store::{EntryBaseline, EntryKind};
use crate::ports::removal::{RemovalError, RemovalPort};
use crate::ports::trash_store::{
    DeleteApply, DeleteRecord, DeleteStore, TrashManifestEntry, TrashStoreError,
};
use crate::recovery::NeedsRecoveryReason;

/// 一批数据库软删除的条目上限。
///
/// 它是“有界批次任务”的边界：一棵大树按它切片，事务里的每条语句最多处理这么多条目，`changes` 一条
/// 批次仍然装下整棵树的事件（事件不必分页，design §7.2）。
pub const MAX_DELETE_BATCH_ENTRIES: usize = 256;

/// 回收站保留期：删除条目的可恢复内容从删除时起至少保留 30 天（design §10.3）。
pub const TRASH_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

/// 子树 token：服务端按冻结清单算出的 128 位摘要。
///
/// 它不是安全凭据，而是乐观并发证明：任一成员的任一字段（身份、父目录、名称、种类、revision、内容
/// 版本）变化，或增删成员，都会得到不同的 token。客户端把上次拿到的 token 带回来，核心重算后比较，
/// 就能证明“从我看见到我要删，这棵树没变过”（T12）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubtreeToken([u8; ID_LEN]);

impl SubtreeToken {
    /// 由字节构造（证据解码与适配器读回用）。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
        SubtreeToken(bytes)
    }

    /// 原始字节。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }

    /// 规范文本形式（32 个小写十六进制字符）。
    #[must_use]
    pub fn to_hex(self) -> String {
        hex_encode(&self.0)
    }

    /// 解析规范文本；长度或字符不合规返回原因（不猜一个 token）。
    ///
    /// # Errors
    ///
    /// 文本不是 32 个小写十六进制字符时返回原因文本。
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let bytes = hex_decode(text)?;
        let mut raw = [0u8; ID_LEN];
        if bytes.len() != ID_LEN {
            return Err(format!(
                "子树 token 文本长度 {} 字节，应为 {ID_LEN}",
                bytes.len()
            ));
        }
        raw.copy_from_slice(&bytes);
        Ok(SubtreeToken(raw))
    }

    /// 按冻结清单计算 token（见类型文档）。
    #[must_use]
    pub fn of_image(image: &SubtreeImage) -> Self {
        // FNV-1a 128：不引第三方依赖、与成员顺序相关、对任一字段敏感；token 只用于并发检测。
        const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
        let mut state = OFFSET;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                state ^= u128::from(*byte);
                state = state.wrapping_mul(PRIME);
            }
        };
        feed(b"vaultsyncd.subtree_token.v1");
        feed(image.root.as_bytes());
        feed(&(image.entries.len() as u64).to_be_bytes());
        for item in &image.entries {
            feed(item.entry.as_bytes());
            match item.parent {
                Some(parent) => {
                    feed(b"parent");
                    feed(parent.as_bytes());
                }
                None => feed(b"no_parent"),
            }
            feed(&(item.path.len() as u64).to_be_bytes());
            feed(item.path.as_bytes());
            feed(item.kind.as_text().as_bytes());
            feed(&item.revision.get().to_be_bytes());
            match item.content_version {
                Some(version) => {
                    feed(b"version");
                    feed(version.as_bytes());
                }
                None => feed(b"no_version"),
            }
        }
        SubtreeToken(state.to_be_bytes())
    }
}

impl fmt::Display for SubtreeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// 冻结清单里的一条：子树中的一个条目及其原位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeEntry {
    /// 条目身份。
    pub entry: EntryId,
    /// 父目录条目（合成根条目不能删除，因此子树根也有父目录）。
    pub parent: Option<EntryId>,
    /// Root 内相对路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 当前 revision（事务里比较并写的基线）。
    pub revision: Revision,
    /// 当前内容版本；目录为 `None`。
    pub content_version: Option<VersionId>,
}

/// 冻结的一棵逻辑子树：目标条目自身 + 所有活跃后代。
///
/// 顺序由端口决定并保持稳定（深度优先）；token、有界批次与回收站清单都从它派生，因此“检查并发”
/// 与“写什么”用的是同一份事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeImage {
    root: EntryId,
    entries: Vec<SubtreeEntry>,
}

impl SubtreeImage {
    /// 校验并构造清单。
    ///
    /// 不变量：至少一条；恰好一条的 `entry` 等于 `root`；没有重复身份；每条路径都是合法的 Root 内
    /// 相对路径；非根成员的父目录必须在清单里（否则不是一棵连通的子树）。
    ///
    /// # Errors
    ///
    /// 违反上述不变量时返回原因文本（适配器缺陷或坏行，调用方按契约违规处理）。
    pub fn new(root: EntryId, entries: Vec<SubtreeEntry>) -> Result<Self, String> {
        if entries.is_empty() {
            return Err("子树清单不能为空".to_owned());
        }
        let roots = entries.iter().filter(|item| item.entry == root).count();
        if roots != 1 {
            return Err(format!(
                "子树清单里根条目 {root} 出现 {roots} 次，应恰好一次"
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &entries {
            if !seen.insert(item.entry) {
                return Err(format!("子树清单里条目 {} 重复", item.entry));
            }
            validate_path(&item.path).map_err(|detail| format!("条目路径不合法：{detail}"))?;
        }
        for item in &entries {
            match item.parent {
                None if item.entry == root => {
                    return Err("子树根条目缺少父目录：Root 根条目不能被删除".to_owned());
                }
                None => return Err(format!("条目 {} 缺少父目录", item.entry)),
                Some(parent) if item.entry != root && !seen.contains(&parent) => {
                    return Err(format!(
                        "条目 {} 的父目录 {parent} 不在清单里：清单不是连通的子树",
                        item.entry
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(SubtreeImage { root, entries })
    }

    /// 子树根条目。
    #[must_use]
    pub fn root(&self) -> EntryId {
        self.root
    }

    /// 清单内容（顺序稳定）。
    #[must_use]
    pub fn entries(&self) -> &[SubtreeEntry] {
        &self.entries
    }

    /// 条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（构造时已保证非空）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 子树 token。
    #[must_use]
    pub fn token(&self) -> SubtreeToken {
        SubtreeToken::of_image(self)
    }

    /// 切成有界批次：每批至多 `limit` 条，保持清单顺序。
    ///
    /// # Errors
    ///
    /// `limit` 为 0 时返回原因：没有边界的批次等于一条整树语句。
    pub fn batches(&self, limit: usize) -> Result<Vec<DeleteBatch>, String> {
        if limit == 0 {
            return Err("批次上限不能是 0".to_owned());
        }
        Ok(self
            .entries
            .chunks(limit)
            .map(|chunk| DeleteBatch {
                entries: chunk.to_vec(),
            })
            .collect())
    }

    /// 按身份找一条清单项。
    #[must_use]
    pub fn find(&self, entry: EntryId) -> Option<&SubtreeEntry> {
        self.entries.iter().find(|item| item.entry == entry)
    }
}

/// 一批有界条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteBatch {
    /// 本批条目（至多 `MAX_DELETE_BATCH_ENTRIES` 条）。
    pub entries: Vec<SubtreeEntry>,
}

/// 意图前提：删除开始时条目在现场与库里的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteSnapshot {
    /// 条目身份。
    pub entry: EntryId,
    /// 条目 revision（事务内比较并写的基线，十进制无符号值）。
    pub revision: u64,
    /// 删除时的父目录条目。
    pub parent: EntryId,
    /// Root 内相对路径。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 实测身份（`st_dev`/`st_ino`）：恢复时用来证明“路径上还是那一个条目”。
    pub identity: FileKey,
    /// 冻结清单的 token。
    pub token: SubtreeToken,
    /// 冻结清单的条目数。
    pub entries: u64,
}

impl DeleteSnapshot {
    /// `expected_before` 的规范 JSON 文本（路径按 UTF-8 十六进制，与命名空间证据同一套编码器）。
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                r#"{{"present":true"#,
                r#","entry":"{}""#,
                r#","revision":"{}""#,
                r#","parent":"{}""#,
                r#","path_hex":"{}""#,
                r#","kind":"{}""#,
                r#","size":"{}""#,
                r#","device":"{}""#,
                r#","inode":"{}""#,
                r#","links":"{}""#,
                r#","token":"{}""#,
                r#","entries":"{}"}}"#
            ),
            self.entry.to_hex(),
            self.revision,
            self.parent.to_hex(),
            hex_encode(self.path.as_bytes()),
            self.kind.as_text(),
            self.identity.bytes,
            self.identity.device,
            self.identity.inode,
            self.identity.links,
            self.token.to_hex(),
            self.entries
        )
    }

    /// 从 `expected_before` 的 JSON 文本还原；不符合本模块编码就报错，不猜现场。
    ///
    /// # Errors
    ///
    /// 见 [`StateJsonError`]：不是 JSON object、缺字段、类型不符、未知键、非法值。
    pub fn from_json(text: &str) -> Result<Self, StateJsonError> {
        let mut fields = parse_flat_object(text)?;
        if !take_bool(&mut fields, "present")? {
            return Err(StateJsonError::InvalidValue {
                field: "present",
                detail: "删除意图的前提必须存在".to_owned(),
            });
        }
        let entry = take_entry(&mut fields, "entry")?;
        let revision = take_u64(&mut fields, "revision")?;
        let parent = take_entry(&mut fields, "parent")?;
        let path = take_path(&mut fields, "path_hex")?;
        let kind = take_kind(&mut fields, "kind")?;
        let bytes = take_u64(&mut fields, "size")?;
        let device = take_u64(&mut fields, "device")?;
        let inode = take_u64(&mut fields, "inode")?;
        let links = u32::try_from(take_u64(&mut fields, "links")?).map_err(|_| {
            StateJsonError::InvalidValue {
                field: "links",
                detail: "超出 u32".to_owned(),
            }
        })?;
        let token = take_token(&mut fields, "token")?;
        let entries = take_u64(&mut fields, "entries")?;
        finish_object(
            &fields,
            DeleteSnapshot {
                entry,
                revision,
                parent,
                path,
                kind,
                identity: FileKey {
                    device,
                    inode,
                    links,
                    bytes,
                },
                token,
                entries,
            },
        )
    }

    /// 现场的条目是否还是前提描述的那一个（同种类、同 inode）。
    #[must_use]
    pub fn matches(&self, state: &PathState) -> bool {
        kind_matches(self.kind, state) && state.same_inode_as(&self.identity)
    }
}

/// 意图目标：整棵子树不再出现在逻辑命名空间里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteAfter {
    /// 本次逻辑删除的条目数（冻结清单长度）。
    pub entries: u64,
}

impl DeleteAfter {
    /// `desired_after` 的规范 JSON 文本：删除的目标是“不存在”。
    #[must_use]
    pub fn to_json(self) -> String {
        format!(r#"{{"present":false,"entries":"{}"}}"#, self.entries)
    }

    /// 从 `desired_after` 的 JSON 文本还原；删除之外的目标形态不是删除意图。
    ///
    /// # Errors
    ///
    /// 见 [`StateJsonError`]；`present: true` 不是合法的删除目标。
    pub fn from_json(text: &str) -> Result<Self, StateJsonError> {
        let mut fields = parse_flat_object(text)?;
        if take_bool(&mut fields, "present")? {
            return Err(StateJsonError::InvalidValue {
                field: "present",
                detail: "删除意图的目标必须是不存在".to_owned(),
            });
        }
        let entries = take_u64(&mut fields, "entries")?;
        finish_object(&fields, DeleteAfter { entries })
    }
}

/// 一条新删除意图的输入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDeleteIntent {
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 现场前提。
    pub before: DeleteSnapshot,
    /// 目标。
    pub after: DeleteAfter,
}

impl NewDeleteIntent {
    /// 用 `prepared` 阶段表示这条意图。
    #[must_use]
    pub fn prepared(&self) -> DeleteIntent {
        DeleteIntent {
            intent_id: self.intent_id,
            root: self.root,
            before: self.before.clone(),
            after: self.after,
            phase: PutPhase::Prepared,
        }
    }
}

/// 一条已持久化的删除意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteIntent {
    /// 意图身份。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 现场前提（已解码）。
    pub before: DeleteSnapshot,
    /// 目标（已解码）。
    pub after: DeleteAfter,
    /// 持久化阶段。
    pub phase: PutPhase,
}

impl DeleteIntent {
    /// 子树根条目。
    #[must_use]
    pub fn entry(&self) -> EntryId {
        self.before.entry
    }

    /// 目标路径：`fs_intents.target_path` 列与恢复观察都用它。
    #[must_use]
    pub fn target_path(&self) -> &str {
        &self.before.path
    }

    /// 恢复判断用的一句话现场描述。
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "意图 {} 阶段 {} 删除{} {}（{} 项，token {}）",
            self.intent_id,
            self.phase.as_text(),
            self.before.kind.as_text(),
            self.before.path,
            self.after.entries,
            self.before.token
        )
    }
}

/// 子树忙标记的一条：正在删除的子树。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusySubtree {
    /// 所属 Root。
    pub root: RootId,
    /// 子树根条目。
    pub entry: EntryId,
    /// 子树根的 Root 内相对路径。
    pub path: String,
}

impl BusySubtree {
    /// 这个路径是否落在该子树内（子树根自身或其后代）。
    #[must_use]
    pub fn covers(&self, path: &str) -> bool {
        path == self.path || is_descendant(path, &self.path)
    }
}

/// 子树忙的冲突信息：命中哪一棵正在删除的子树。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeBusyConflict {
    /// 正在删除的子树根。
    pub entry: EntryId,
    /// 正在删除的子树路径。
    pub path: String,
}

impl fmt::Display for SubtreeBusyConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "子树 {}（条目 {}）正在删除：等删除任务完成后再写",
            self.path, self.entry
        )
    }
}

/// 正在删除的子树集合：由未完成的删除意图构造，供其他写路径查“忙不忙”。
///
/// 忙是**持久事实**的投影：进程重启后未完成的删除意图仍在 `fs_intents` 里，重建本结构即可继续挡住
/// 该子树的写入，不依赖内存状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubtreeBusy {
    subtrees: Vec<BusySubtree>,
}

impl SubtreeBusy {
    /// 空集合。
    #[must_use]
    pub fn new() -> Self {
        SubtreeBusy::default()
    }

    /// 从尚未收尾的删除意图构造。
    #[must_use]
    pub fn from_intents(intents: &[DeleteIntent]) -> Self {
        let subtrees = intents
            .iter()
            .filter(|intent| intent.phase.is_open())
            .map(|intent| BusySubtree {
                root: intent.root,
                entry: intent.entry(),
                path: intent.target_path().to_owned(),
            })
            .collect();
        SubtreeBusy { subtrees }
    }

    /// 正在删除的子树。
    #[must_use]
    pub fn subtrees(&self) -> &[BusySubtree] {
        &self.subtrees
    }

    /// 该 Root 内的某个路径能否写入：落在任一正在删除的子树里就返回冲突。
    ///
    /// 这是“受影响子树写入返回明确忙状态”的判定入口（design §10.3）；装配层在 put/新建/移动/恢复
    /// 之前调用它，把冲突映射成 `SUBTREE_CHANGED`（可重试，客户端等删除批次完成后重新规划）。
    ///
    /// # Errors
    ///
    /// 命中正在删除的子树时返回 [`SubtreeBusyConflict`]。
    pub fn check_write(&self, root: RootId, path: &str) -> Result<(), SubtreeBusyConflict> {
        self.subtrees
            .iter()
            .find(|busy| busy.root == root && busy.covers(path))
            .map(|busy| SubtreeBusyConflict {
                entry: busy.entry,
                path: busy.path.clone(),
            })
            .map_or(Ok(()), Err)
    }
}

/// 删除请求：目标条目 + revision + （目录的）服务端子树 token。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRequest {
    /// 意图身份（重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 被删除的条目。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端计划时看到的条目 revision（design §10.3“删除要求目标 Revision”）。
    pub expected_revision: Revision,
    /// 服务端上次给出的子树 token；目录删除必填，文件删除必须为空。
    pub subtree_token: Option<SubtreeToken>,
    /// 条目的 Root 内相对路径。
    pub path: String,
}

/// 一次成功删除：事务已提交、意图已收尾、路径实测不存在。
#[derive(Clone, Debug)]
pub struct CommittedDelete {
    /// 元数据事务结果（`phase` 必须是 `finalized`）。
    pub record: DeleteRecord,
    /// 删除后路径的实测现场（必须是 `Absent`）。
    pub target: PathState,
    /// 工作目录里删掉的目录项数（文件按 1 计）。
    pub removed: u64,
    /// 有界批次数（数据库侧与工作目录侧取较大者，便于观察“大树不是一条语句”）。
    pub batches: usize,
}

impl CommittedDelete {
    /// 已收尾的意图身份。
    #[must_use]
    pub fn intent_id(&self) -> IntentId {
        self.record.intent_id
    }
}

/// 删除失败的原因。
#[derive(Clone, Debug)]
pub enum DeleteFailureReason {
    /// 请求本身不成立（路径、token 形态、Root 根条目），未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
    /// 同一 Root 已有提交在进行。
    Gate(CommitGateError),
    /// Root epoch 与请求预期不符（`EPOCH_CHANGED`）。
    EpochChanged {
        /// 请求携带的 epoch。
        expected: ServerEpoch,
        /// Root 当前的 epoch。
        actual: ServerEpoch,
    },
    /// 条目 revision 已不是请求基线（`BASE_MISMATCH`）。
    BaseMismatch {
        /// 原因文本。
        detail: String,
    },
    /// 子树在计划后变化（并发新增、改名、删改；`SUBTREE_CHANGED`，T12）。
    SubtreeChanged {
        /// 原因文本。
        detail: String,
    },
    /// 该子树正在被另一个删除任务处理（明确忙；按 `SUBTREE_CHANGED` 重试）。
    SubtreeBusy {
        /// 冲突信息。
        conflict: SubtreeBusyConflict,
    },
    /// 路径读不到（权限、挂载、IO）：故障不是“不存在”。
    TargetUnreadable(NamespaceError),
    /// 工作目录移除失败：操作保持 `prepared`，不能声称已删除。
    Removal(RemovalError),
    /// 移除报告完成，但路径仍然存在：不推进阶段、不提交事务。
    RemovalIncomplete {
        /// 目标路径。
        path: String,
        /// 已删的目录项数。
        removed: u64,
        /// 实测现场。
        observed: PathState,
    },
    /// 子树清单与请求/基线不一致（适配器或坏行）。
    Manifest {
        /// 原因文本。
        detail: String,
    },
    /// 意图日志失败。
    Intent(IntentLogError),
    /// 存储层失败（基线、清单或删除事务）。
    Store(TrashStoreError),
    /// 存储层报告事务成功，但返回的证据与请求不一致：适配器缺陷，必须停写人工核对。
    StoreContract {
        /// 原因文本。
        detail: String,
    },
}

impl DeleteFailureReason {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            DeleteFailureReason::InvalidRequest { .. } => "invalid_request",
            DeleteFailureReason::Gate(error) => error.code(),
            DeleteFailureReason::EpochChanged { .. } => "epoch_changed",
            DeleteFailureReason::BaseMismatch { .. } => "base_mismatch",
            DeleteFailureReason::SubtreeChanged { .. } => "subtree_changed",
            DeleteFailureReason::SubtreeBusy { .. } => "subtree_busy",
            DeleteFailureReason::TargetUnreadable(error) => error.code(),
            DeleteFailureReason::Removal(error) => error.code(),
            DeleteFailureReason::RemovalIncomplete { .. } => "removal_incomplete",
            DeleteFailureReason::Manifest { .. } => "manifest_mismatch",
            DeleteFailureReason::Intent(error) => error.code(),
            DeleteFailureReason::Store(error) => error.code(),
            DeleteFailureReason::StoreContract { .. } => "store_contract_violation",
        }
    }

    /// wire 错误码（design §15.4）；服务端内部原因没有对应 wire 码时返回 `None`。
    ///
    /// 子树忙与“移除没做完”都按 `SUBTREE_CHANGED` 返回：两者都是“子树当前处于中间状态，等一会儿拉最新
    /// 事实再重新规划”，design §15.4 没有单独的 `SUBTREE_BUSY` 码，新造一个会越出已确认的错误契约。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            DeleteFailureReason::EpochChanged { .. } => Some(ErrorCode::EpochChanged),
            DeleteFailureReason::BaseMismatch { .. } => Some(ErrorCode::BaseMismatch),
            DeleteFailureReason::SubtreeChanged { .. }
            | DeleteFailureReason::SubtreeBusy { .. }
            | DeleteFailureReason::RemovalIncomplete { .. } => Some(ErrorCode::SubtreeChanged),
            DeleteFailureReason::StoreContract { .. } => Some(ErrorCode::NeedsRecovery),
            _ => None,
        }
    }
}

impl fmt::Display for DeleteFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeleteFailureReason::InvalidRequest { detail } => write!(f, "请求不成立：{detail}"),
            DeleteFailureReason::Gate(error) => write!(f, "{error}"),
            DeleteFailureReason::EpochChanged { expected, actual } => write!(
                f,
                "Root epoch 已是 {actual}，请求预期是 {expected}：服务器事实已重建，旧提交作废"
            ),
            DeleteFailureReason::BaseMismatch { detail } => {
                write!(f, "基线已不是请求的 revision：{detail}")
            }
            DeleteFailureReason::SubtreeChanged { detail } => {
                write!(f, "子树在计划后变化：{detail}")
            }
            DeleteFailureReason::SubtreeBusy { conflict } => write!(f, "{conflict}"),
            DeleteFailureReason::TargetUnreadable(error) => write!(f, "{error}"),
            DeleteFailureReason::Removal(error) => write!(f, "{error}"),
            DeleteFailureReason::RemovalIncomplete {
                path,
                removed,
                observed,
            } => write!(
                f,
                "移除报告完成但 {path} 仍是 {observed}（已删 {removed} 项）：不提交事务"
            ),
            DeleteFailureReason::Manifest { detail } => write!(f, "子树清单不可用：{detail}"),
            DeleteFailureReason::Intent(error) => write!(f, "{error}"),
            DeleteFailureReason::Store(error) => write!(f, "{error}"),
            DeleteFailureReason::StoreContract { detail } => write!(
                f,
                "存储层报告提交成功但证据不一致：{detail}（必须停写人工核对）"
            ),
        }
    }
}

impl std::error::Error for DeleteFailureReason {}

/// 删除失败：原因 + 已持久化的意图身份与阶段。
#[derive(Clone, Debug)]
pub struct DeleteFailure {
    /// 失败原因。
    pub reason: DeleteFailureReason,
    /// 已持久化的意图（未写库时为 `None`）。
    pub intent_id: Option<IntentId>,
    /// 最近一次成功持久化的阶段。
    pub phase: Option<PutPhase>,
}

impl DeleteFailure {
    /// 用原因构造“尚未写库”的失败。
    #[must_use]
    pub fn new(reason: DeleteFailureReason) -> Self {
        DeleteFailure {
            reason,
            intent_id: None,
            phase: None,
        }
    }

    /// 用原因与该意图已持久化的阶段构造失败。
    #[must_use]
    pub fn after_intent(reason: DeleteFailureReason, intent: &DeleteIntent) -> Self {
        DeleteFailure {
            reason,
            intent_id: Some(intent.intent_id),
            phase: Some(intent.phase),
        }
    }

    /// 失败是否产生了已提交结果：恒为 `false`（没有元数据事务就没有 committed）。
    #[must_use]
    pub fn is_committed(&self) -> bool {
        false
    }

    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }

    /// wire 错误码（design §15.4）。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        self.reason.error_code()
    }
}

impl fmt::Display for DeleteFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.intent_id, self.phase) {
            (Some(id), Some(phase)) => {
                write!(f, "{}（意图 {id} 停在 {}）", self.reason, phase.as_text())
            }
            _ => write!(f, "{}", self.reason),
        }
    }
}

impl std::error::Error for DeleteFailure {}

/// 提交一次删除（文件或整棵目录子树）。
///
/// 顺序：请求形状 → Root 串行化 → epoch → 基线 revision → 子树忙 → 现场 → 冻结清单与 token → 意图
/// （`prepared`）→ 有界批次移除与复核 → `published` → 单一事务公布 → ACK。无论成功还是失败，退出前
/// 都会释放 Root 票据。
///
/// # Errors
///
/// 见 [`DeleteFailure`]：请求不成立、Root 忙、epoch 变化、基线/revision 过期、token 失效、子树忙、
/// 现场不可读、移除失败或没做完、意图/存储失败、存储契约不一致。失败都不是 committed，未收尾的意图
/// 保留为恢复证据，并继续让该子树保持忙。
pub async fn commit_delete<S, L, N>(
    gate: &mut RootCommitGate,
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    request: &DeleteRequest,
) -> Result<CommittedDelete, Box<DeleteFailure>>
where
    S: DeleteStore + ?Sized,
    L: DeleteIntentLog + ?Sized,
    N: NamespacePort + RemovalPort + ?Sized,
{
    validate_request(request)?;
    let ticket = gate
        .acquire(request.root)
        .map_err(|error| failure(DeleteFailureReason::Gate(error)))?;
    let outcome = commit_under_ticket(store, log, ns, at, request).await;
    gate.release(ticket);
    outcome
}

async fn commit_under_ticket<S, L, N>(
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    request: &DeleteRequest,
) -> Result<CommittedDelete, Box<DeleteFailure>>
where
    S: DeleteStore + ?Sized,
    L: DeleteIntentLog + ?Sized,
    N: NamespacePort + RemovalPort + ?Sized,
{
    // 1. epoch：恢复过的服务器不是同一套事实。
    let facts = store
        .root_facts(request.root)
        .await
        .map_err(store_failure)?;
    if facts.epoch != request.expected_epoch {
        return Err(failure(DeleteFailureReason::EpochChanged {
            expected: request.expected_epoch,
            actual: facts.epoch,
        }));
    }

    // 2. 基线：条目存在、revision 是请求的那一份、不是 Root 的合成根条目。
    let baseline = match store.entry_baseline(request.root, request.entry).await {
        Ok(baseline) => baseline,
        Err(TrashStoreError::EntryMissing { entry }) => {
            return Err(base_mismatch(format!("条目 {entry} 已不存在或已删除")));
        }
        Err(TrashStoreError::UnknownRoot { root }) => {
            return Err(base_mismatch(format!("Root {root} 已不存在")));
        }
        Err(error) => return Err(store_failure(error)),
    };
    check_baseline(&baseline, request)?;

    // 3. 子树忙：未完成的删除意图（含别的会话、上一轮崩溃留下的）挡住同一子树。
    let open = log
        .unfinished_deletes(request.root)
        .await
        .map_err(intent_failure)?;
    SubtreeBusy::from_intents(&open)
        .check_write(request.root, &request.path)
        .map_err(|conflict| failure(DeleteFailureReason::SubtreeBusy { conflict }))?;

    // 4. 现场：种类与基线一致，才谈得上“删的是同一个东西”。
    let state = inspect(ns, &request.path)?;
    if !kind_matches(baseline.kind, &state) {
        return Err(failure(DeleteFailureReason::SubtreeChanged {
            detail: format!(
                "路径 {} 现在是 {}，基线是{}",
                request.path,
                state.state_name(),
                baseline.kind.as_text()
            ),
        }));
    }
    let identity = *state.identity().ok_or_else(|| {
        failure(DeleteFailureReason::SubtreeChanged {
            detail: format!("路径 {} 没有可比较的身份", request.path),
        })
    })?;

    // 5. 冻结清单与 token：并发新增让前提失败（T12）。
    let image = store
        .subtree_image(request.root, request.entry)
        .await
        .map_err(store_failure)?;
    check_image(&image, &baseline, &request.path)?;
    check_token(request, baseline.kind, &image)?;
    // Root 的合成根条目没有父目录，不能删（删了就没有可删的 Root）。
    let parent = image
        .find(baseline.entry)
        .and_then(|item| item.parent)
        .ok_or_else(|| {
            invalid_request(format!(
                "条目 {} 是 Root 的合成根条目：根条目不能被删除",
                baseline.entry
            ))
        })?;

    // 6. 先持久意图：写不进去就什么都不动。
    let new = NewDeleteIntent {
        intent_id: request.intent_id,
        root: request.root,
        before: DeleteSnapshot {
            entry: baseline.entry,
            revision: baseline.revision.get(),
            parent,
            path: request.path.clone(),
            kind: baseline.kind,
            identity,
            token: image.token(),
            entries: image.len() as u64,
        },
        after: DeleteAfter {
            entries: image.len() as u64,
        },
    };
    let prepared = log.begin_delete(&new, at).await.map_err(intent_failure)?;

    // 7. 有界批次移除工作目录里的内容，再复核现场。
    let removal = match remove_from_workdir(ns, &request.path, baseline.kind) {
        Ok(stats) => stats,
        Err(reason) => {
            return Err(Box::new(DeleteFailure::after_intent(reason, &prepared)));
        }
    };
    let target = match inspect(ns, &request.path) {
        Ok(PathState::Absent) => PathState::Absent,
        Ok(observed) => {
            return Err(Box::new(DeleteFailure::after_intent(
                DeleteFailureReason::RemovalIncomplete {
                    path: request.path.clone(),
                    removed: removal.removed,
                    observed,
                },
                &prepared,
            )));
        }
        Err(failure) => {
            return Err(Box::new(DeleteFailure::after_intent(
                failure.reason,
                &prepared,
            )));
        }
    };

    // 8. published：磁盘已删，事务还没提交。
    let published = log
        .advance_delete(prepared.intent_id, PutPhase::Published, at)
        .await
        .map_err(|error| {
            DeleteFailure::after_intent(DeleteFailureReason::Intent(error), &prepared)
        })?;

    // 9. 单一事务公布：回收站清单 + 墓碑 + 分批软删除 + 事件批次 + 意图收尾。
    let apply = delete_apply(&published, &image, at)?;
    let batches = apply.batches.len();
    let record = store
        .apply_delete(&apply)
        .await
        .map_err(|error| match error {
            TrashStoreError::RevisionChanged { expected, actual } => DeleteFailure::after_intent(
                DeleteFailureReason::BaseMismatch {
                    detail: format!("提交时条目 revision 已从 {expected} 变为 {actual}"),
                },
                &published,
            ),
            TrashStoreError::ManifestMismatch { detail } => {
                DeleteFailure::after_intent(DeleteFailureReason::Manifest { detail }, &published)
            }
            error => DeleteFailure::after_intent(DeleteFailureReason::Store(error), &published),
        })?;
    check_record(&record, &published, image.len())?;
    Ok(CommittedDelete {
        record,
        target,
        removed: removal.removed,
        batches: batches.max(removal.batches),
    })
}

fn validate_request(request: &DeleteRequest) -> Result<(), Box<DeleteFailure>> {
    validate_path(&request.path).map_err(invalid_request)?;
    if request.path.is_empty() {
        return Err(invalid_request("路径不能为空"));
    }
    Ok(())
}

fn check_baseline(
    baseline: &EntryBaseline,
    request: &DeleteRequest,
) -> Result<(), Box<DeleteFailure>> {
    if baseline.entry != request.entry || baseline.root != request.root {
        return Err(base_mismatch(format!(
            "库里的条目 {}（Root {}）不是请求条目 {}",
            baseline.entry, baseline.root, request.entry
        )));
    }
    if baseline.revision != request.expected_revision {
        return Err(base_mismatch(format!(
            "条目 revision 已是 {}，请求基线是 {}",
            baseline.revision, request.expected_revision
        )));
    }
    Ok(())
}

/// 冻结清单必须真的以目标条目为根、路径与请求一致。
fn check_image(
    image: &SubtreeImage,
    baseline: &EntryBaseline,
    path: &str,
) -> Result<(), Box<DeleteFailure>> {
    if image.root() != baseline.entry {
        return Err(failure(DeleteFailureReason::Manifest {
            detail: format!(
                "清单的根是 {}，不是请求条目 {}",
                image.root(),
                baseline.entry
            ),
        }));
    }
    let root = image.find(baseline.entry).ok_or_else(|| {
        failure(DeleteFailureReason::Manifest {
            detail: "清单缺少目标条目自身".to_owned(),
        })
    })?;
    if root.path != path {
        return Err(failure(DeleteFailureReason::Manifest {
            detail: format!("清单里目标路径是 {:?}，请求是 {path:?}", root.path),
        }));
    }
    if root.kind != baseline.kind || root.revision != baseline.revision {
        return Err(failure(DeleteFailureReason::SubtreeChanged {
            detail: format!(
                "清单里目标条目是 {}/revision {}，基线是 {}/revision {}",
                root.kind.as_text(),
                root.revision,
                baseline.kind.as_text(),
                baseline.revision
            ),
        }));
    }
    // 子条目的路径必须以目标路径为前缀：否则清单混进了别的子树。
    for item in image.entries() {
        if item.entry != baseline.entry && !is_descendant(&item.path, path) {
            return Err(failure(DeleteFailureReason::Manifest {
                detail: format!("清单条目路径 {:?} 不在子树 {path:?} 之下", item.path),
            }));
        }
        if item.kind == EntryKind::Directory && item.content_version.is_some() {
            return Err(failure(DeleteFailureReason::Manifest {
                detail: format!("目录条目 {} 带着内容版本", item.entry),
            }));
        }
    }
    Ok(())
}

/// 目录删除必须携带正确的子树 token；文件删除不带 token（两种形态不混用）。
fn check_token(
    request: &DeleteRequest,
    kind: EntryKind,
    image: &SubtreeImage,
) -> Result<(), Box<DeleteFailure>> {
    let token = image.token();
    match (kind, request.subtree_token) {
        (EntryKind::File, None) => Ok(()),
        (EntryKind::File, Some(_)) => Err(invalid_request(format!(
            "{} 是普通文件：文件删除不带子树 token",
            request.path
        ))),
        (EntryKind::Directory, None) => Err(invalid_request(format!(
            "{} 是目录：目录删除必须携带服务端给出的子树 token",
            request.path
        ))),
        (EntryKind::Directory, Some(expected)) if expected == token => Ok(()),
        (EntryKind::Directory, Some(expected)) => {
            Err(failure(DeleteFailureReason::SubtreeChanged {
                detail: format!(
                    "子树 token 已从 {expected} 变为 {token}：冻结/计划之后这棵树有新增或变化"
                ),
            }))
        }
    }
}

/// 工作目录移除的执行结果。
#[derive(Clone, Copy, Debug, Default)]
struct RemovalRun {
    removed: u64,
    batches: usize,
}

/// 按有界批次把路径从工作目录移除；失败原样上抛（不改数据库、不推进阶段）。
fn remove_from_workdir<N>(
    ns: &mut N,
    path: &str,
    kind: EntryKind,
) -> Result<RemovalRun, DeleteFailureReason>
where
    N: NamespacePort + RemovalPort + ?Sized,
{
    match kind {
        EntryKind::File => {
            ns.remove_file(path).map_err(DeleteFailureReason::Removal)?;
            Ok(RemovalRun {
                removed: 1,
                batches: 1,
            })
        }
        EntryKind::Directory => {
            let mut run = RemovalRun::default();
            loop {
                let progress = ns
                    .remove_directory_batch(path, MAX_DELETE_BATCH_ENTRIES)
                    .map_err(DeleteFailureReason::Removal)?;
                run.removed += progress.removed;
                run.batches += 1;
                if progress.complete {
                    return Ok(run);
                }
                if progress.removed == 0 {
                    // 一批什么都没删又不算完成：再调下去只会空转，按“没做完”停在这里。
                    return Err(DeleteFailureReason::Manifest {
                        detail: format!("{path} 的移除批次既没有进展也没有报告完成"),
                    });
                }
            }
        }
    }
}

/// 从意图证据拼出删除事务的输入（提交与恢复共用，避免两条路径写出不同的事务）。
fn delete_apply(
    intent: &DeleteIntent,
    image: &SubtreeImage,
    at: UtcTimestamp,
) -> Result<DeleteApply, Box<DeleteFailure>> {
    let revision = Revision::new(intent.before.revision).map_err(|error| {
        DeleteFailure::after_intent(
            DeleteFailureReason::StoreContract {
                detail: format!("意图里的 revision 不合法：{error}"),
            },
            intent,
        )
    })?;
    let root_revision = image.find(intent.before.entry).map(|item| item.revision);
    if root_revision != Some(revision) {
        return Err(Box::new(DeleteFailure::after_intent(
            DeleteFailureReason::SubtreeChanged {
                detail: format!(
                    "清单里目标 revision 与意图基线 {} 不一致",
                    intent.before.revision
                ),
            },
            intent,
        )));
    }
    let batches = image.batches(MAX_DELETE_BATCH_ENTRIES).map_err(|detail| {
        DeleteFailure::after_intent(DeleteFailureReason::Manifest { detail }, intent)
    })?;
    let trash = trash_manifest(image, at).map_err(|detail| {
        DeleteFailure::after_intent(DeleteFailureReason::Manifest { detail }, intent)
    })?;
    Ok(DeleteApply {
        intent_id: intent.intent_id,
        root: intent.root,
        entry: intent.entry(),
        batches,
        trash,
        at,
    })
}

/// 回收站清单：每个条目一条，带 30 天保留期的到期时间。
fn trash_manifest(
    image: &SubtreeImage,
    at: UtcTimestamp,
) -> Result<Vec<TrashManifestEntry>, String> {
    let expires_at = at
        .as_unix_seconds()
        .checked_add(TRASH_RETENTION_SECONDS)
        .ok_or_else(|| "删除时间加保留期超出范围".to_owned())
        .and_then(|seconds| {
            UtcTimestamp::from_unix_seconds(seconds).map_err(|error| error.to_string())
        })?;
    image
        .entries()
        .iter()
        .map(|item| {
            let parent = item
                .parent
                .ok_or_else(|| format!("条目 {} 缺少父目录：不能写进回收站清单", item.entry))?;
            if item.path.is_empty() {
                return Err(format!("条目 {} 的路径为空", item.entry));
            }
            Ok(TrashManifestEntry {
                entry: item.entry,
                parent,
                path: item.path.clone(),
                kind: item.kind,
                revision: item.revision,
                content_version: item.content_version,
                deleted_at: at,
                expires_at,
            })
        })
        .collect()
}

/// 事务声称成功时核对返回的证据：不一致说明适配器违反端口契约，必须停写人工核对，不能 ACK。
fn check_record(
    record: &DeleteRecord,
    intent: &DeleteIntent,
    entries: usize,
) -> Result<(), Box<DeleteFailure>> {
    let problem = if record.intent_id != intent.intent_id {
        Some(format!(
            "事务收尾的意图 {} 不是请求意图 {}",
            record.intent_id, intent.intent_id
        ))
    } else if !record.phase.is_finished() {
        Some(format!("意图阶段是 {} 而不是 finalized", record.phase))
    } else if record.entry != intent.entry() || record.root != intent.root {
        Some(format!(
            "事务写的是 Root {} 条目 {}，不是请求的 Root {} 条目 {}",
            record.root,
            record.entry,
            intent.root,
            intent.entry()
        ))
    } else if record.trashed != entries as u64 {
        Some(format!(
            "幽灵删除：事务报告进回收站 {} 条，清单是 {entries} 条",
            record.trashed
        ))
    } else {
        None
    };
    match problem {
        Some(detail) => Err(Box::new(DeleteFailure::after_intent(
            DeleteFailureReason::StoreContract { detail },
            intent,
        ))),
        None => Ok(()),
    }
}

fn inspect<N>(ns: &mut N, path: &str) -> Result<PathState, Box<DeleteFailure>>
where
    N: NamespacePort + ?Sized,
{
    ns.inspect(path)
        .map_err(|error| failure(DeleteFailureReason::TargetUnreadable(error)))
}

fn kind_matches(kind: EntryKind, state: &PathState) -> bool {
    matches!(
        (kind, state),
        (EntryKind::File, PathState::File { .. })
            | (EntryKind::Directory, PathState::Directory { .. })
    )
}

fn take_token(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<SubtreeToken, StateJsonError> {
    let text = take_text(fields, field)?;
    SubtreeToken::from_hex(&text).map_err(|detail| StateJsonError::InvalidValue { field, detail })
}

fn failure(reason: DeleteFailureReason) -> Box<DeleteFailure> {
    Box::new(DeleteFailure::new(reason))
}

fn store_failure(error: TrashStoreError) -> Box<DeleteFailure> {
    failure(DeleteFailureReason::Store(error))
}

fn intent_failure(error: IntentLogError) -> Box<DeleteFailure> {
    failure(DeleteFailureReason::Intent(error))
}

fn base_mismatch(detail: impl Into<String>) -> Box<DeleteFailure> {
    failure(DeleteFailureReason::BaseMismatch {
        detail: detail.into(),
    })
}

fn invalid_request(detail: impl Into<String>) -> Box<DeleteFailure> {
    failure(DeleteFailureReason::InvalidRequest {
        detail: detail.into(),
    })
}

/// 恢复时对目标路径的观察结果。
#[derive(Clone, Debug)]
pub struct ObservedDelete {
    /// 目标路径的观察；错误必须原样传进来，不能先变成“不存在”。
    pub target: Result<PathState, NamespaceError>,
}

/// 现场是第三种状态时，需要协调的具体情形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteReconcileReason {
    /// 路径上现在是别的条目（身份/种类不是前提那个）：不覆盖、不删，重新规划。
    TargetOccupied,
    /// 目标祖先目录不存在：路径已经不可达，但仍需人工确认是不是同一个子树。
    TargetUnavailable,
    /// 库里这棵子树已经全部软删除（回收站清单已写）：只剩意图阶段没收尾，交人工核对。
    SubtreeAlreadyDeleted,
}

impl DeleteReconcileReason {
    /// 稳定短名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DeleteReconcileReason::TargetOccupied => "target_occupied",
            DeleteReconcileReason::TargetUnavailable => "target_unavailable",
            DeleteReconcileReason::SubtreeAlreadyDeleted => "subtree_already_deleted",
        }
    }
}

impl fmt::Display for DeleteReconcileReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条未完成删除意图的恢复结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteRecoveryDecision {
    /// 意图已 `finalized`：不需要动作。
    AlreadyFinished,
    /// 现场还是前提那个条目：重做有界批次移除并补完事务。
    Redo,
    /// 路径已经不存在：只补完元数据事务。
    Complete,
    /// 现场无法用意图解释但可以保留：等协调，不覆盖、不提交。
    Reconcile {
        /// 具体情形。
        reason: DeleteReconcileReason,
    },
    /// 没有可证明的现场：落盘 `needs_recovery` 并暂停该 Root 的破坏性写入。
    NeedsRecovery {
        /// 具体原因。
        reason: NeedsRecoveryReason,
    },
}

impl DeleteRecoveryDecision {
    /// 稳定短名（恢复报告与日志用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeleteRecoveryDecision::AlreadyFinished => "already_finished",
            DeleteRecoveryDecision::Redo => "redo",
            DeleteRecoveryDecision::Complete => "complete",
            DeleteRecoveryDecision::Reconcile { reason } => reason.as_str(),
            DeleteRecoveryDecision::NeedsRecovery { reason } => reason.as_str(),
        }
    }

    /// 是否必须暂停该 Root 的破坏性写入（只有 `NeedsRecovery`）。
    #[must_use]
    pub fn pauses_writes(self) -> bool {
        matches!(self, DeleteRecoveryDecision::NeedsRecovery { .. })
    }
}

impl fmt::Display for DeleteRecoveryDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 按持久阶段与目标现场判定一条删除意图该怎么处理。
///
/// 只读意图证据与实测现场，不写库、不动磁盘。删除是单向动作：路径还在且身份与前提一致才重做；
/// 路径已经不在只补事务；被换成别的条目就保留现场（不删别人的东西）。
#[must_use]
pub fn classify_delete(intent: &DeleteIntent, observed: &ObservedDelete) -> DeleteRecoveryDecision {
    if intent.phase == PutPhase::Finalized {
        return DeleteRecoveryDecision::AlreadyFinished;
    }
    if intent.phase == PutPhase::NeedsRecovery {
        return DeleteRecoveryDecision::NeedsRecovery {
            reason: NeedsRecoveryReason::MarkedNeedsRecovery,
        };
    }
    match &observed.target {
        Err(_) => DeleteRecoveryDecision::NeedsRecovery {
            reason: NeedsRecoveryReason::TargetUnreadable,
        },
        Ok(PathState::Absent) => DeleteRecoveryDecision::Complete,
        Ok(PathState::AncestorMissing) => DeleteRecoveryDecision::Reconcile {
            reason: DeleteReconcileReason::TargetUnavailable,
        },
        Ok(state) if intent.before.matches(state) => DeleteRecoveryDecision::Redo,
        Ok(_) => DeleteRecoveryDecision::Reconcile {
            reason: DeleteReconcileReason::TargetOccupied,
        },
    }
}

/// 一条意图的恢复结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteRecoveryOutcome {
    /// 现场还是前提那个条目：重做了移除并补完事务。
    Redone {
        /// 子树根条目（未变）。
        entry: EntryId,
        /// 本次逻辑删除的条目数。
        entries: u64,
    },
    /// 路径已不存在：只补完元数据事务。
    Completed {
        /// 子树根条目。
        entry: EntryId,
        /// 本次逻辑删除的条目数。
        entries: u64,
    },
    /// 现场无法解释但可以保留：等协调，不覆盖、不提交。
    Preserved {
        /// 具体情形。
        reason: DeleteReconcileReason,
    },
    /// 没有可证明的现场：意图已落 `needs_recovery`，该 Root 暂停破坏性写入。
    NeedsRecovery {
        /// 具体原因。
        reason: NeedsRecoveryReason,
    },
    /// 恢复动作本身失败：没有完成也没有假装完成，Root 保持关闭，下次启动重试。
    Unresolved {
        /// 稳定错误码。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
}

impl DeleteRecoveryOutcome {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            DeleteRecoveryOutcome::Redone { .. } => "redone",
            DeleteRecoveryOutcome::Completed { .. } => "completed",
            DeleteRecoveryOutcome::Preserved { reason } => reason.as_str(),
            DeleteRecoveryOutcome::NeedsRecovery { reason } => reason.as_str(),
            DeleteRecoveryOutcome::Unresolved { code, .. } => code,
        }
    }

    /// 是否产生了已提交的删除。
    #[must_use]
    pub fn is_committed(&self) -> bool {
        matches!(
            self,
            DeleteRecoveryOutcome::Redone { .. } | DeleteRecoveryOutcome::Completed { .. }
        )
    }

    /// 是否让该 Root 保持关闭。
    #[must_use]
    pub fn pauses_writes(&self) -> bool {
        matches!(
            self,
            DeleteRecoveryOutcome::NeedsRecovery { .. } | DeleteRecoveryOutcome::Unresolved { .. }
        )
    }
}

impl fmt::Display for DeleteRecoveryOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeleteRecoveryOutcome::Redone { entry, entries } => {
                write!(f, "已重做删除，子树 {entry} 的 {entries} 个条目进入回收站")
            }
            DeleteRecoveryOutcome::Completed { entry, entries } => write!(
                f,
                "已补完删除事务，子树 {entry} 的 {entries} 个条目进入回收站"
            ),
            DeleteRecoveryOutcome::Preserved { reason } => {
                write!(f, "保留现场等协调（{reason}）")
            }
            DeleteRecoveryOutcome::NeedsRecovery { reason } => {
                write!(f, "暂停该 Root 的破坏性写入（{reason}）")
            }
            DeleteRecoveryOutcome::Unresolved { code, detail } => {
                write!(f, "恢复未完成（{code}）：{detail}")
            }
        }
    }
}

/// 一条被处理过的未完成删除意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredDeleteIntent {
    /// 恢复前的意图记录。
    pub intent: DeleteIntent,
    /// 按恢复前证据得到的判定。
    pub decision: DeleteRecoveryDecision,
    /// 实际执行的恢复结果。
    pub outcome: DeleteRecoveryOutcome,
}

/// 一个 Root 的删除恢复报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRecoveryReport {
    /// 被恢复的 Root。
    pub root: RootId,
    /// 每条未完成意图的处理结果（保持意图写入顺序）。
    pub intents: Vec<RecoveredDeleteIntent>,
}

impl DeleteRecoveryReport {
    /// 是否有任何一条意图要求暂停该 Root 的破坏性写入。
    #[must_use]
    pub fn pauses_writes(&self) -> bool {
        self.intents
            .iter()
            .any(|entry| entry.outcome.pauses_writes())
    }

    /// 是否有需要协调（保留双方）的意图。
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.intents
            .iter()
            .any(|entry| matches!(entry.outcome, DeleteRecoveryOutcome::Preserved { .. }))
    }

    /// 本次恢复产生提交的意图数。
    #[must_use]
    pub fn committed(&self) -> usize {
        self.intents
            .iter()
            .filter(|entry| entry.outcome.is_committed())
            .count()
    }
}

/// 删除恢复的失败。
#[derive(Clone, Debug)]
pub enum DeleteRecoveryError {
    /// 读不出未完成意图（越权、存储失败）。
    Intent(IntentLogError),
}

impl DeleteRecoveryError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            DeleteRecoveryError::Intent(error) => error.code(),
        }
    }
}

impl fmt::Display for DeleteRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeleteRecoveryError::Intent(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for DeleteRecoveryError {}

/// 按未完成删除意图恢复一个 Root。
///
/// 与 put/命名空间的恢复同一套不变量：只重放能证明的动作，现场无法解释就保留，读不到就落盘
/// `needs_recovery` 停写，动作本身失败就保持关闭等下次启动。补事务时重新读一次子树清单，并要求它的
/// token 仍等于意图里的 token——冻结之后又出现新条目属于异常，不悄悄一起删掉。
///
/// # Errors
///
/// 见 [`DeleteRecoveryError`]：读不出未完成意图。单条意图的失败作为
/// [`DeleteRecoveryOutcome::Unresolved`] 留在报告里。
pub async fn recover_delete_root<L, N, S>(
    log: &mut L,
    ns: &mut N,
    store: &mut S,
    at: UtcTimestamp,
    root: RootId,
) -> Result<DeleteRecoveryReport, DeleteRecoveryError>
where
    L: DeleteIntentLog + ?Sized,
    N: NamespacePort + RemovalPort + ?Sized,
    S: DeleteStore + ?Sized,
{
    let unfinished = log
        .unfinished_deletes(root)
        .await
        .map_err(DeleteRecoveryError::Intent)?;
    let mut intents = Vec::with_capacity(unfinished.len());
    for intent in unfinished {
        if !intent.phase.is_open() {
            continue;
        }
        let observed = ObservedDelete {
            target: ns.inspect(intent.target_path()),
        };
        let decision = classify_delete(&intent, &observed);
        let outcome = apply_decision(log, ns, store, at, &intent, decision).await;
        intents.push(RecoveredDeleteIntent {
            intent,
            decision,
            outcome,
        });
    }
    Ok(DeleteRecoveryReport { root, intents })
}

async fn apply_decision<L, N, S>(
    log: &mut L,
    ns: &mut N,
    store: &mut S,
    at: UtcTimestamp,
    intent: &DeleteIntent,
    decision: DeleteRecoveryDecision,
) -> DeleteRecoveryOutcome
where
    L: DeleteIntentLog + ?Sized,
    N: NamespacePort + RemovalPort + ?Sized,
    S: DeleteStore + ?Sized,
{
    match decision {
        DeleteRecoveryDecision::Redo => {
            if let Err(reason) = remove_from_workdir(ns, intent.target_path(), intent.before.kind) {
                return unresolved(reason.code(), reason.to_string());
            }
            match inspect(ns, intent.target_path()) {
                Ok(PathState::Absent) => {}
                Ok(observed) => {
                    return unresolved(
                        "removal_incomplete",
                        format!("重做移除后 {} 仍是 {observed}", intent.target_path()),
                    );
                }
                Err(failure) => return unresolved(failure.code(), failure.to_string()),
            }
            complete_metadata(log, store, at, intent).await
        }
        DeleteRecoveryDecision::Complete => complete_metadata(log, store, at, intent).await,
        DeleteRecoveryDecision::Reconcile { reason } => DeleteRecoveryOutcome::Preserved { reason },
        DeleteRecoveryDecision::NeedsRecovery { reason } => {
            if intent.phase == PutPhase::NeedsRecovery {
                DeleteRecoveryOutcome::NeedsRecovery { reason }
            } else {
                match log
                    .advance_delete(intent.intent_id, PutPhase::NeedsRecovery, at)
                    .await
                {
                    Ok(_) => DeleteRecoveryOutcome::NeedsRecovery { reason },
                    Err(error) => unresolved(error.code(), error.to_string()),
                }
            }
        }
        DeleteRecoveryDecision::AlreadyFinished => unresolved(
            "intent_already_finished",
            "未完成清单里出现已收尾意图".to_owned(),
        ),
    }
}

/// 补完崩溃前没提交的删除事务（回收站清单/墓碑/软删除/事件批次/意图收尾，同一事务）。
async fn complete_metadata<L, S>(
    log: &mut L,
    store: &mut S,
    at: UtcTimestamp,
    intent: &DeleteIntent,
) -> DeleteRecoveryOutcome
where
    L: DeleteIntentLog + ?Sized,
    S: DeleteStore + ?Sized,
{
    let image = match store.subtree_image(intent.root, intent.entry()).await {
        Ok(image) => image,
        Err(TrashStoreError::EntryMissing { .. }) => {
            return DeleteRecoveryOutcome::Preserved {
                reason: DeleteReconcileReason::SubtreeAlreadyDeleted,
            };
        }
        Err(error) => return unresolved(error.code(), error.to_string()),
    };
    if image.token() != intent.before.token || image.len() as u64 != intent.after.entries {
        return unresolved(
            "subtree_changed",
            format!(
                "恢复时的子树 token/条目数与意图不符：冻结 {}，现在 {}",
                intent.before.entries,
                image.len()
            ),
        );
    }
    let published = match advance_if_prepared(log, intent, at).await {
        Ok(published) => published,
        Err(outcome) => return outcome,
    };
    let apply = match delete_apply(&published, &image, at) {
        Ok(apply) => apply,
        Err(failure) => return unresolved(failure.code(), failure.to_string()),
    };
    match store.apply_delete(&apply).await {
        Ok(record) => match check_record(&record, &published, image.len()) {
            Ok(()) => DeleteRecoveryOutcome::Completed {
                entry: record.entry,
                entries: record.trashed,
            },
            Err(failure) => unresolved(failure.code(), failure.to_string()),
        },
        Err(error) => unresolved(error.code(), error.to_string()),
    }
}

/// 阶段还停在 `prepared` 时先按现场证据补记 `published`（崩溃可能停在“移除完成、阶段没记下来”）。
async fn advance_if_prepared<L>(
    log: &mut L,
    intent: &DeleteIntent,
    at: UtcTimestamp,
) -> Result<DeleteIntent, DeleteRecoveryOutcome>
where
    L: DeleteIntentLog + ?Sized,
{
    if intent.phase != PutPhase::Prepared {
        return Ok(intent.clone());
    }
    log.advance_delete(intent.intent_id, PutPhase::Published, at)
        .await
        .map_err(|error| unresolved(error.code(), error.to_string()))
}

fn unresolved(code: &'static str, detail: String) -> DeleteRecoveryOutcome {
    DeleteRecoveryOutcome::Unresolved { code, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{
        FakeDeleteStore, FakeIntentLog, FakeNamespace, batch_entries, entry_id, epoch, intent_id,
        revision, root_id, subtree_image, user_time, version_id,
    };

    fn root() -> RootId {
        root_id(0x21)
    }

    /// 被删除的目录 `docs`（revision 3）及其两个文件、一个子目录与一个深层文件。
    fn dir_image() -> SubtreeImage {
        subtree_image(
            entry_id(0x62),
            entry_id(0x61),
            &[
                (0x62, "docs", EntryKind::Directory, 3),
                (0x63, "docs/a.txt", EntryKind::File, 1),
                (0x64, "docs/b.txt", EntryKind::File, 2),
                (0x65, "docs/sub", EntryKind::Directory, 1),
                (0x66, "docs/sub/c.txt", EntryKind::File, 1),
            ],
        )
    }

    /// 被删除的普通文件 `notes.txt`（revision 2，父目录 0x61）。
    fn file_image() -> SubtreeImage {
        subtree_image(
            entry_id(0x71),
            entry_id(0x61),
            &[(0x71, "notes.txt", EntryKind::File, 2)],
        )
    }

    fn file_request() -> DeleteRequest {
        DeleteRequest {
            intent_id: intent_id(0x41),
            root: root(),
            entry: entry_id(0x71),
            expected_epoch: epoch(3),
            expected_revision: revision(2),
            subtree_token: None,
            path: "notes.txt".to_owned(),
        }
    }

    fn dir_request(token: Option<SubtreeToken>) -> DeleteRequest {
        DeleteRequest {
            intent_id: intent_id(0x42),
            root: root(),
            entry: entry_id(0x62),
            expected_epoch: epoch(3),
            expected_revision: revision(3),
            subtree_token: token,
            path: "docs".to_owned(),
        }
    }

    /// 工作目录现场：`docs` 及其四个成员 + 根下的 `notes.txt`。
    fn workdir() -> FakeNamespace {
        FakeNamespace::new()
            .with_directory("docs")
            .with_directory("docs/sub")
            .with_file("docs/a.txt")
            .with_file("docs/b.txt")
            .with_file("docs/sub/c.txt")
            .with_file("notes.txt")
    }

    struct Fixture {
        gate: RootCommitGate,
        store: FakeDeleteStore,
        log: FakeIntentLog,
        ns: FakeNamespace,
    }

    fn fixture(image: SubtreeImage) -> Fixture {
        let store = FakeDeleteStore::new().with_image(image);
        let trace = store.trace.clone();
        Fixture {
            gate: RootCommitGate::new(),
            store,
            log: FakeIntentLog {
                trace,
                ..FakeIntentLog::default()
            },
            ns: workdir(),
        }
    }

    async fn run(
        fixture: &mut Fixture,
        request: &DeleteRequest,
    ) -> Result<CommittedDelete, Box<DeleteFailure>> {
        commit_delete(
            &mut fixture.gate,
            &mut fixture.store,
            &mut fixture.log,
            &mut fixture.ns,
            user_time(1_000),
            request,
        )
        .await
    }

    /// ��迹里第一次出现某行的位置（断言顺序用）。
    fn store_position(fixture: &Fixture, needle: &str) -> usize {
        fixture
            .store
            .trace
            .entries()
            .iter()
            .position(|line| line == needle)
            .unwrap_or_else(|| panic!("轨迹里没有 {needle}"))
    }

    #[tokio::test]
    async fn file_delete_writes_trash_manifest_in_one_batch() {
        let mut fixture = fixture(file_image());
        let committed = run(&mut fixture, &file_request())
            .await
            .expect("删除应成功");

        assert_eq!(committed.record.trashed, 1);
        assert_eq!(committed.target, PathState::Absent);
        assert_eq!(committed.removed, 1);
        assert_eq!(committed.record.intent_id, intent_id(0x41));
        assert!(
            !fixture.ns.paths.contains_key("notes.txt"),
            "工作目录里的文件应已删除"
        );

        // 顺序：先持久意图，再删磁盘，再 published，最后单一事务。
        let removed_at = fixture
            .ns
            .trace
            .entries()
            .iter()
            .position(|line| line == "namespace remove_file notes.txt")
            .expect("应调用移除");
        assert!(
            store_position(&fixture, "intent begin delete prepared")
                < store_position(&fixture, "store apply_delete"),
            "先持久意图，最后才是元数据事务"
        );
        assert_eq!(
            fixture.log.delete_intents[0].phase,
            PutPhase::Published,
            "核心把意图推进到 published；finalized 由存储层在同一事务里收尾"
        );
        assert_eq!(committed.record.phase, PutPhase::Finalized);
        let _ = removed_at;

        let apply = &fixture.store.writes[0];
        assert_eq!(apply.entry, entry_id(0x71));
        assert_eq!(apply.batches.len(), 1);
        assert_eq!(apply.entries(), 1);
        let trash = &apply.trash[0];
        assert_eq!(trash.path, "notes.txt");
        assert_eq!(trash.parent, entry_id(0x61));
        assert_eq!(trash.revision, revision(2));
        assert_eq!(trash.content_version, Some(version_id(0x71)));
        assert_eq!(trash.deleted_at, user_time(1_000));
        assert_eq!(
            trash.expires_at.as_unix_seconds(),
            1_000 + TRASH_RETENTION_SECONDS,
            "回收站保留期是 30 天"
        );
        // 意图证据：token 与条目数都从冻结清单派生。
        let intent = &fixture.log.delete_intents[0];
        assert_eq!(intent.before.token, file_image().token());
        assert_eq!(intent.before.entries, 1);
        assert_eq!(intent.after.entries, 1);
    }

    #[tokio::test]
    async fn directory_delete_removes_in_bounded_batches() {
        let mut fixture = fixture(dir_image());
        // 假适配器每批只处理 2 项：核心必须循环到报完成为止。
        fixture.ns.removal_batch_limit = 2;
        let committed = run(&mut fixture, &dir_request(Some(dir_image().token())))
            .await
            .expect("目录删除应成功");

        assert_eq!(committed.record.trashed, 5);
        assert_eq!(committed.removed, 5, "子树根的 4 个成员 + 目录自身");
        assert!(committed.batches >= 1);
        assert!(
            fixture.ns.removal_batches >= 2,
            "4 个成员按每批 2 项至少需要 2 次调用，实际 {}",
            fixture.ns.removal_batches
        );
        assert_eq!(fixture.ns.state_of("docs"), PathState::Absent);
        assert_eq!(fixture.store.writes[0].trash.len(), 5);
        assert_eq!(
            batch_entries(&fixture.store.writes[0].batches),
            dir_image()
                .entries()
                .iter()
                .map(|item| item.entry)
                .collect::<Vec<_>>(),
            "批次必须完整覆盖冻结清单的每一项"
        );
    }

    #[test]
    fn batches_are_bounded_and_cover_the_image() {
        let image = dir_image();
        let batches = image.batches(2).expect("批次合法");
        assert_eq!(batches.len(), 3);
        assert!(batches.iter().all(|batch| batch.entries.len() <= 2));
        assert_eq!(batch_entries(&batches).len(), image.len());
        assert!(image.batches(0).is_err(), "0 条上限不是有界批次");
    }

    #[tokio::test]
    async fn concurrent_child_addition_invalidates_the_token() {
        // 客户端看到的是“没有深层文件”的旧 token：并发新增了 docs/sub/c.txt。
        let stale = subtree_image(
            entry_id(0x62),
            entry_id(0x61),
            &[
                (0x62, "docs", EntryKind::Directory, 3),
                (0x63, "docs/a.txt", EntryKind::File, 1),
                (0x64, "docs/b.txt", EntryKind::File, 2),
                (0x65, "docs/sub", EntryKind::Directory, 1),
            ],
        )
        .token();
        assert_ne!(stale, dir_image().token());

        let mut fixture = fixture(dir_image());
        let failure = run(&mut fixture, &dir_request(Some(stale)))
            .await
            .expect_err("token 失效必须拒绝");
        assert_eq!(failure.code(), "subtree_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert!(fixture.log.delete_intents.is_empty(), "前提失败不写意图");
        assert_eq!(fixture.ns.removal_batches, 0, "前提失败不动磁盘");
        assert!(fixture.store.writes.is_empty());
        assert!(!failure.is_committed());
        // 并发新增的文件还在：不吞掉它。
        assert!(fixture.ns.state_of("docs/sub/c.txt").is_file());
    }

    #[tokio::test]
    async fn directory_delete_without_token_is_rejected() {
        let mut fixture = fixture(dir_image());
        let failure = run(&mut fixture, &dir_request(None))
            .await
            .expect_err("目录删除必须带 token");
        assert_eq!(failure.code(), "invalid_request");
        assert!(fixture.log.delete_intents.is_empty());
        assert_eq!(fixture.ns.removal_batches, 0);
    }

    #[tokio::test]
    async fn file_delete_rejects_subtree_token() {
        let mut fixture = fixture(file_image());
        let mut request = file_request();
        request.subtree_token = Some(file_image().token());
        let failure = run(&mut fixture, &request)
            .await
            .expect_err("文件删除不带 token");
        assert_eq!(failure.code(), "invalid_request");
        assert!(fixture.log.delete_intents.is_empty());
    }

    #[tokio::test]
    async fn stale_revision_is_a_base_mismatch() {
        let mut fixture = fixture(file_image());
        let mut request = file_request();
        request.expected_revision = revision(1);
        let failure = run(&mut fixture, &request)
            .await
            .expect_err("revision 过期必须拒绝");
        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
        assert!(fixture.log.delete_intents.is_empty());
        assert_eq!(fixture.ns.removal_batches, 0);
    }

    #[tokio::test]
    async fn unreadable_path_is_not_absence() {
        let mut fixture = fixture(file_image());
        fixture.ns.inspect_error = Some(NamespaceError::Unreadable {
            detail: "挂载不可读".to_owned(),
        });
        let failure = run(&mut fixture, &file_request())
            .await
            .expect_err("读不到不能当不存在");
        assert_eq!(failure.code(), "target_unreadable");
        assert!(fixture.log.delete_intents.is_empty());
        assert_eq!(fixture.ns.removal_batches, 0);
    }

    #[tokio::test]
    async fn removal_failure_is_never_committed() {
        let mut fixture = fixture(file_image());
        fixture.ns.remove_error = Some(RemovalError::InUse {
            path: "notes.txt".to_owned(),
            detail: "文件被占用".to_owned(),
        });
        let failure = run(&mut fixture, &file_request())
            .await
            .expect_err("移除失败不能 ACK");
        assert_eq!(failure.code(), "path_in_use");
        assert!(!failure.is_committed());
        assert_eq!(failure.phase, Some(PutPhase::Prepared), "意图停在 prepared");
        assert!(fixture.store.writes.is_empty(), "没有事务就没有提交");
        assert!(
            fixture.ns.state_of("notes.txt").is_file(),
            "文件还在，不谎报已删"
        );
    }

    #[tokio::test]
    async fn removal_that_keeps_the_path_is_not_committed() {
        let mut fixture = fixture(dir_image());
        fixture.ns.removal_keeps_path = true;
        let failure = run(&mut fixture, &dir_request(Some(dir_image().token())))
            .await
            .expect_err("移除没做完不能提交");
        assert_eq!(failure.code(), "removal_incomplete");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.ns.state_of("docs").is_directory());
    }

    #[tokio::test]
    async fn open_delete_intent_keeps_the_subtree_busy() {
        let mut fixture = fixture(dir_image());
        let busy_intent = NewDeleteIntent {
            intent_id: intent_id(0x40),
            root: root(),
            before: DeleteSnapshot {
                entry: entry_id(0x62),
                revision: 3,
                parent: entry_id(0x61),
                path: "docs".to_owned(),
                kind: EntryKind::Directory,
                identity: FileKey {
                    device: 7,
                    inode: 100,
                    links: 2,
                    bytes: 64,
                },
                token: dir_image().token(),
                entries: 5,
            },
            after: DeleteAfter { entries: 5 },
        };
        fixture.log.delete_intents.push(busy_intent.prepared());

        let failure = run(&mut fixture, &dir_request(Some(dir_image().token())))
            .await
            .expect_err("删除中的子树必须报忙");
        assert_eq!(failure.code(), "subtree_busy");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert_eq!(fixture.log.delete_intents.len(), 1, "不写第二条意图");
        assert_eq!(fixture.ns.removal_batches, 0);

        // 忙判定本身：子树根与后代忙，兄弟路径与其他 Root 不忙。
        let busy = SubtreeBusy::from_intents(&fixture.log.delete_intents);
        assert!(busy.check_write(root(), "docs").is_err());
        assert!(busy.check_write(root(), "docs/sub/c.txt").is_err());
        assert!(busy.check_write(root(), "docs2").is_ok());
        assert!(busy.check_write(root_id(0x22), "docs").is_ok());
        assert_eq!(busy.subtrees().len(), 1);
        // 收尾后的意图不再挡路。
        fixture.log.delete_intents[0].phase = PutPhase::Finalized;
        assert!(
            SubtreeBusy::from_intents(&fixture.log.delete_intents)
                .check_write(root(), "docs")
                .is_ok()
        );
    }

    #[tokio::test]
    async fn store_record_mismatch_is_a_contract_violation() {
        let mut fixture = fixture(file_image());
        fixture.store.record = Some(DeleteRecord {
            intent_id: intent_id(0x41),
            phase: PutPhase::Finalized,
            root: root(),
            entry: entry_id(0x71),
            trashed: 0,
            seq: crate::test_support::root_seq(1),
            tx: crate::test_support::tx_id(0x71),
        });
        let failure = run(&mut fixture, &file_request())
            .await
            .expect_err("清单与事务不一致不能 ACK");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
    }

    #[tokio::test]
    async fn store_failure_after_publish_stays_published() {
        let mut fixture = fixture(file_image());
        fixture.store.apply_error = Some(TrashStoreError::Storage {
            detail: "磁盘写满".to_owned(),
        });
        let failure = run(&mut fixture, &file_request())
            .await
            .expect_err("事务失败不能 ACK");
        assert_eq!(failure.code(), "trash_store_failed");
        assert_eq!(failure.phase, Some(PutPhase::Published));
        assert!(!failure.is_committed());
    }

    /// 恢复证据用的现场身份：从工作目录假端口取，不写死 inode。
    fn prepared_intent(fixture: &Fixture, image: &SubtreeImage) -> DeleteIntent {
        let root = image.find(image.root()).expect("清单有根条目");
        let identity = *fixture
            .ns
            .state_of(&root.path)
            .identity()
            .expect("恢复前提要求路径存在");
        prepared_delete_intent(image, identity)
    }

    fn prepared_delete_intent(image: &SubtreeImage, identity: FileKey) -> DeleteIntent {
        let item = image.find(image.root()).expect("清单有根条目");
        NewDeleteIntent {
            intent_id: intent_id(0x43),
            root: root(),
            before: DeleteSnapshot {
                entry: item.entry,
                revision: item.revision.get(),
                parent: item.parent.expect("子树根有父目录"),
                path: item.path.clone(),
                kind: item.kind,
                identity,
                token: image.token(),
                entries: image.len() as u64,
            },
            after: DeleteAfter {
                entries: image.len() as u64,
            },
        }
        .prepared()
    }

    #[tokio::test]
    async fn recovery_redoes_the_removal_when_the_target_is_still_there() {
        let image = dir_image();
        let mut fixture = fixture(image.clone());
        let intent = prepared_intent(&fixture, &image);
        fixture.log.delete_intents.push(intent);
        fixture.ns.removal_batch_limit = 2;

        let report = recover_delete_root(
            &mut fixture.log,
            &mut fixture.ns,
            &mut fixture.store,
            user_time(1_000),
            root(),
        )
        .await
        .expect("恢复应读取成功");
        assert_eq!(report.intents.len(), 1);
        assert_eq!(report.intents[0].decision, DeleteRecoveryDecision::Redo);
        assert!(report.intents[0].outcome.is_committed());
        assert_eq!(report.committed(), 1);
        assert!(!report.pauses_writes());
        assert_eq!(fixture.ns.state_of("docs"), PathState::Absent);
        assert_eq!(fixture.store.writes.len(), 1);
        assert_eq!(fixture.store.writes[0].trash.len(), 5);
        assert_eq!(fixture.log.delete_intents[0].phase, PutPhase::Published);
    }

    #[tokio::test]
    async fn recovery_completes_when_the_path_is_already_gone() {
        let image = dir_image();
        let mut fixture = fixture(image.clone());
        let intent = prepared_intent(&fixture, &image);
        fixture.log.delete_intents.push(intent);
        for path in [
            "docs",
            "docs/sub",
            "docs/a.txt",
            "docs/b.txt",
            "docs/sub/c.txt",
        ] {
            fixture.ns.paths.remove(path);
        }

        let report = recover_delete_root(
            &mut fixture.log,
            &mut fixture.ns,
            &mut fixture.store,
            user_time(1_000),
            root(),
        )
        .await
        .expect("恢复应读取成功");
        assert_eq!(report.intents[0].decision, DeleteRecoveryDecision::Complete);
        assert!(report.intents[0].outcome.is_committed());
        assert_eq!(fixture.ns.removal_batches, 0, "不需要再删磁盘");
        assert_eq!(fixture.store.writes.len(), 1);
        assert_eq!(fixture.log.delete_intents[0].phase, PutPhase::Published);
    }

    #[tokio::test]
    async fn recovery_preserves_when_the_target_was_replaced() {
        let image = dir_image();
        let mut fixture = fixture(image.clone());
        let intent = prepared_intent(&fixture, &image);
        fixture.log.delete_intents.push(intent);
        // 路径上已经不是那个目录（被换成了普通文件）。
        fixture.ns.paths.remove("docs");
        fixture.ns.paths.insert(
            "docs".to_owned(),
            (
                EntryKind::File,
                FileKey {
                    device: 7,
                    inode: 777,
                    links: 1,
                    bytes: 9,
                },
            ),
        );

        let report = recover_delete_root(
            &mut fixture.log,
            &mut fixture.ns,
            &mut fixture.store,
            user_time(1_000),
            root(),
        )
        .await
        .expect("恢复应读取成功");
        assert_eq!(
            report.intents[0].decision,
            DeleteRecoveryDecision::Reconcile {
                reason: DeleteReconcileReason::TargetOccupied
            }
        );
        assert!(report.needs_reconciliation());
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.ns.state_of("docs").is_file(), "不动别人的新内容");
    }

    #[tokio::test]
    async fn recovery_with_unreadable_target_marks_needs_recovery() {
        let image = file_image();
        let mut fixture = fixture(image.clone());
        let intent = prepared_intent(&fixture, &image);
        fixture.log.delete_intents.push(intent);
        fixture.ns.inspect_error = Some(NamespaceError::Unreadable {
            detail: "挂载掉线".to_owned(),
        });

        let report = recover_delete_root(
            &mut fixture.log,
            &mut fixture.ns,
            &mut fixture.store,
            user_time(1_000),
            root(),
        )
        .await
        .expect("恢复应读取成功");
        assert!(report.pauses_writes());
        assert_eq!(fixture.log.delete_intents[0].phase, PutPhase::NeedsRecovery);
        assert!(fixture.store.writes.is_empty());
    }

    #[tokio::test]
    async fn recovery_refuses_to_swallow_new_children() {
        let image = file_image();
        let mut fixture = fixture(image.clone());
        let intent = prepared_intent(&fixture, &image);
        fixture.log.delete_intents.push(intent);
        fixture.ns.paths.remove("notes.txt");
        // 冻结之后库里又出现了一个子条目：清单不再等于意图里的那一份。
        fixture.store.images.insert(
            entry_id(0x71),
            subtree_image(
                entry_id(0x71),
                entry_id(0x61),
                &[
                    (0x71, "notes.txt", EntryKind::File, 2),
                    (0x72, "notes.txt/x", EntryKind::File, 1),
                ],
            ),
        );

        let report = recover_delete_root(
            &mut fixture.log,
            &mut fixture.ns,
            &mut fixture.store,
            user_time(1_000),
            root(),
        )
        .await
        .expect("恢复应读取成功");
        assert!(report.pauses_writes(), "异常必须停写人工核对");
        assert_eq!(report.intents[0].outcome.as_str(), "subtree_changed");
        assert!(fixture.store.writes.is_empty());
    }

    #[test]
    fn delete_evidence_json_round_trips() {
        let snapshot = DeleteSnapshot {
            entry: entry_id(0x62),
            revision: 3,
            parent: entry_id(0x61),
            path: "docs/带 \"引号\" 的名字".to_owned(),
            kind: EntryKind::Directory,
            identity: FileKey {
                device: 7,
                inode: 41,
                links: 2,
                bytes: 64,
            },
            token: dir_image().token(),
            entries: 5,
        };
        let json = snapshot.to_json();
        assert_eq!(
            DeleteSnapshot::from_json(&json).expect("往返一致"),
            snapshot
        );

        let after = DeleteAfter { entries: 5 };
        assert_eq!(
            DeleteAfter::from_json(&after.to_json()).expect("往返一致"),
            after
        );
        assert!(DeleteAfter::from_json(r#"{"present":true,"entries":"5"}"#).is_err());
        assert!(DeleteSnapshot::from_json(r#"{"present":false}"#).is_err());
        assert!(DeleteSnapshot::from_json(r#"{"present":true,"entry":"01"}"#).is_err());
    }

    #[test]
    fn token_changes_with_any_member_field() {
        let base = dir_image();
        assert_eq!(base.token(), dir_image().token(), "同一份清单 token 稳定");

        let bumped = subtree_image(
            entry_id(0x62),
            entry_id(0x61),
            &[
                (0x62, "docs", EntryKind::Directory, 4),
                (0x63, "docs/a.txt", EntryKind::File, 1),
                (0x64, "docs/b.txt", EntryKind::File, 2),
                (0x65, "docs/sub", EntryKind::Directory, 1),
                (0x66, "docs/sub/c.txt", EntryKind::File, 1),
            ],
        );
        assert_ne!(base.token(), bumped.token(), "revision 变化必须换 token");

        let renamed = subtree_image(
            entry_id(0x62),
            entry_id(0x61),
            &[
                (0x62, "docs", EntryKind::Directory, 3),
                (0x63, "docs/a-renamed.txt", EntryKind::File, 1),
                (0x64, "docs/b.txt", EntryKind::File, 2),
                (0x65, "docs/sub", EntryKind::Directory, 1),
                (0x66, "docs/sub/c.txt", EntryKind::File, 1),
            ],
        );
        assert_ne!(base.token(), renamed.token(), "改名必须换 token");
        assert_eq!(
            base.token().to_hex().len(),
            vaultsyncd_domain::ids::ID_HEX_LEN
        );
        assert_eq!(
            SubtreeToken::from_hex(&base.token().to_hex()).expect("往返一致"),
            base.token()
        );
        assert!(SubtreeToken::from_hex("zz").is_err());
        assert!(SubtreeToken::from_hex("0011").is_err());
    }

    #[test]
    fn image_rejects_unconnected_and_rootless_manifests() {
        let orphan = SubtreeEntry {
            entry: entry_id(0x63),
            parent: Some(entry_id(0x99)),
            path: "docs/a.txt".to_owned(),
            kind: EntryKind::File,
            revision: revision(1),
            content_version: Some(version_id(0x63)),
        };
        let root_entry = SubtreeEntry {
            entry: entry_id(0x62),
            parent: Some(entry_id(0x61)),
            path: "docs".to_owned(),
            kind: EntryKind::Directory,
            revision: revision(3),
            content_version: None,
        };
        assert!(SubtreeImage::new(entry_id(0x62), vec![root_entry.clone(), orphan]).is_err());
        assert!(SubtreeImage::new(entry_id(0x62), Vec::new()).is_err());
        assert!(
            SubtreeImage::new(entry_id(0x62), vec![root_entry.clone(), root_entry.clone()])
                .is_err()
        );
        let rootless = SubtreeEntry {
            parent: None,
            ..root_entry
        };
        assert!(SubtreeImage::new(entry_id(0x62), vec![rootless]).is_err());
    }
}
