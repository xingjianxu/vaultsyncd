//! 意图日志端口：把 `fs_intents` 的持久化能力暴露给提交状态机（design §11.2 步骤 5、§11.3）。
//!
//! 同一张表覆盖三类提交：[`IntentLog`] 是内容 put（`kind = 'put_content'`），[`NamespaceIntentLog`]
//! 是目录创建与同 Root 移动（`kind = 'create'`/`'move'`），[`DeleteIntentLog`] 是删除（`kind =
//! 'delete'`）。三者共用阶段取值与转换规则（[`PutPhase`]）、共用“先写 `prepared` 意图再动磁盘”的
//! 顺序，只是证据 JSON 的形态不同：put 记内容摘要，命名空间记位置、种类与 inode，删除记子树 token 与
//! 条目数。
//!
//! 端口只做“写一条 prepared 意图、推进阶段、按身份读回、按 Root 列出未完成意图”，不解释业务语义：
//! 什么时候能推进、现场是否满足前提，都由核心判定（[`crate::commit::intent`]、
//! [`crate::recovery`]）。适配器（服务端存储层）必须把每次写入放在自己的事务里，并如实返回行内容；
//! 读到的行不合规要报 [`IntentLogError::MalformedIntent`]，不要猜一个阶段或一条路径。

use std::fmt;

use vaultsyncd_domain::ids::{IntentId, RootId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::commit::delete::{DeleteIntent, NewDeleteIntent};
use crate::commit::intent::{NewPutIntent, PutIntent, PutPhase};
use crate::commit::namespace::{NamespaceIntent, NewNamespaceIntent};

/// 意图日志的失败。
#[derive(Clone, Debug)]
pub enum IntentLogError {
    /// 存储层失败（SQLite 等），保留原始错误文本。
    Storage {
        /// 原始错误文本。
        detail: String,
    },
    /// Root 或意图不在调用方的 owner scope 内。
    OutOfScope,
    /// 意图不存在。
    UnknownIntent {
        /// 被查询的意图。
        id: IntentId,
    },
    /// 意图身份已被占用。
    DuplicateIntent {
        /// 冲突的意图。
        id: IntentId,
    },
    /// 阶段转换不被允许（持久化记录与状态机规则不一致）。
    IllegalTransition {
        /// 已记录的阶段。
        from: PutPhase,
        /// 请求推进到的阶段。
        to: PutPhase,
    },
    /// 写入被存储层约束拒绝（路径、JSON 形态等）：核心应在写库前就拦住，走到这里是缺陷或绕过。
    Rejected {
        /// 稳定短名，例如 `invalid_target_path`。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
    /// 读到的行无法还原成 [`PutIntent`]（阶段文本、JSON 证据或身份长度不合规）。
    MalformedIntent {
        /// 原因文本。
        detail: String,
    },
}

impl IntentLogError {
    /// 稳定错误码（日志、API 映射与验收脚本共用）。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            IntentLogError::Storage { .. } => "intent_storage_failed",
            IntentLogError::OutOfScope => "out_of_scope",
            IntentLogError::UnknownIntent { .. } => "unknown_intent",
            IntentLogError::DuplicateIntent { .. } => "duplicate_intent",
            IntentLogError::IllegalTransition { .. } => "illegal_intent_transition",
            IntentLogError::Rejected { code, .. } => code,
            IntentLogError::MalformedIntent { .. } => "malformed_intent",
        }
    }
}

impl fmt::Display for IntentLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntentLogError::Storage { detail } => write!(f, "意图存储失败：{detail}"),
            IntentLogError::OutOfScope => f.write_str("Root 或意图不在调用方的 owner scope 内"),
            IntentLogError::UnknownIntent { id } => write!(f, "意图 {id} 不存在"),
            IntentLogError::DuplicateIntent { id } => write!(f, "意图身份 {id} 已被占用"),
            IntentLogError::IllegalTransition { from, to } => {
                write!(f, "意图不能从 {} 转到 {}", from.as_text(), to.as_text())
            }
            IntentLogError::Rejected { code, detail } => {
                write!(f, "意图写入被拒绝（{code}）：{detail}")
            }
            IntentLogError::MalformedIntent { detail } => write!(f, "意图行无法还原：{detail}"),
        }
    }
}

impl std::error::Error for IntentLogError {}

/// 意图日志端口。
///
/// 用 `async fn` 是为了直接对接服务端的异步 SQLx 事务；因此本 trait 不是 dyn 兼容的，核心的编排
/// 函数以泛型参数接受实现者（测试用内存假实现，正式路径用存储层适配器）。
// 该 trait 只在本 crate 的编排函数里以泛型参数使用（不外发、不做 dyn 对象），因此接受
// `async fn`；改成返回 `impl Future + Send` 的写法会锁死将来的签名演进，收益不足。
#[allow(async_fn_in_trait)]
pub trait IntentLog {
    /// 写入一条 `prepared` 意图并返回持久化后的记录。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：存储失败、越权、身份冲突、约束拒绝。
    async fn begin_put(
        &mut self,
        new: &NewPutIntent,
        at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError>;

    /// 把意图推进到 `next` 并返回推进后的记录；非法转换必须报错，不得跳过阶段。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：存储失败、意图不存在/越权、非法转换、时间倒退。
    async fn advance_put(
        &mut self,
        id: IntentId,
        next: PutPhase,
        at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError>;

    /// 按身份读一条意图；不存在或越权返回 `None`。
    ///
    /// # Errors
    ///
    /// 存储失败或行无法还原。
    async fn load_put(&mut self, id: IntentId) -> Result<Option<PutIntent>, IntentLogError>;

    /// 按 Root 列出尚未收尾的意图（`prepared`/`published`/`needs_recovery`），按写入顺序。
    ///
    /// 这是启动恢复的入口（design §11.3：先恢复未完成意图再开放写入）。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：越权（不能返回空列表冒充“没有未完成意图”）、存储失败。
    async fn unfinished_puts(&mut self, root: RootId) -> Result<Vec<PutIntent>, IntentLogError>;
}

/// 命名空间意图日志端口：目录创建与同 Root 移动的 `fs_intents` 行（`kind = 'create'`/`'move'`）。
///
/// 契约与 [`IntentLog`] 一致：每次写入在自己的事务里完成，读到的行不合规报
/// [`IntentLogError::MalformedIntent`]，不猜阶段；`target_path` 写目标路径、`staging_path` 为 NULL
/// （命名空间操作没有暂存文件）、`expected_before`/`desired_after` 写命名空间证据 JSON。
// `#[allow(async_fn_in_trait)]`：与 `IntentLog` 同理，只在编排函数里以泛型参数使用。
#[allow(async_fn_in_trait)]
pub trait NamespaceIntentLog {
    /// 写入一条 `prepared` 命名空间意图并返回持久化后的记录。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：存储失败、越权、身份冲突、约束拒绝。
    async fn begin_namespace(
        &mut self,
        new: &NewNamespaceIntent,
        at: UtcTimestamp,
    ) -> Result<NamespaceIntent, IntentLogError>;

    /// 把命名空间意图推进到 `next` 并返回推进后的记录；非法转换必须报错。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：存储失败、意图不存在/越权、非法转换、时间倒退。
    async fn advance_namespace(
        &mut self,
        id: IntentId,
        next: PutPhase,
        at: UtcTimestamp,
    ) -> Result<NamespaceIntent, IntentLogError>;

    /// 按身份读一条命名空间意图；不存在或越权返回 `None`。
    ///
    /// # Errors
    ///
    /// 存储失败或行无法还原。
    async fn load_namespace(
        &mut self,
        id: IntentId,
    ) -> Result<Option<NamespaceIntent>, IntentLogError>;

    /// 按 Root 列出尚未收尾的命名空间意图（`prepared`/`published`/`needs_recovery`），按写入顺序。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：越权（不能返回空列表冒充“没有未完成意图”）、存储失败。
    async fn unfinished_namespaces(
        &mut self,
        root: RootId,
    ) -> Result<Vec<NamespaceIntent>, IntentLogError>;
}

/// 删除意图日志端口：删除的 `fs_intents` 行（`kind = 'delete'`）。
///
/// 契约与 [`IntentLog`] 一致：每次写入在自己的事务里完成，读到的行不合规报
/// [`IntentLogError::MalformedIntent`]，不猜阶段；`target_path` 写被删条目的路径、`staging_path` 为
/// NULL（删除没有暂存文件）、`expected_before`/`desired_after` 写删除证据 JSON。
///
/// [`unfinished_deletes`](DeleteIntentLog::unfinished_deletes) 同时是“子树忙”的事实来源：未收尾的
/// 删除意图代表该子树正在被删除（design §10.3），重启后依旧有效。
// `#[allow(async_fn_in_trait)]`：与 `IntentLog` 同理，只在编排函数里以泛型参数使用。
#[allow(async_fn_in_trait)]
pub trait DeleteIntentLog {
    /// 写入一条 `prepared` 删除意图并返回持久化后的记录。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：存储失败、越权、身份冲突、约束拒绝。
    async fn begin_delete(
        &mut self,
        new: &NewDeleteIntent,
        at: UtcTimestamp,
    ) -> Result<DeleteIntent, IntentLogError>;

    /// 把删除意图推进到 `next` 并返回推进后的记录；非法转换必须报错。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：存储失败、意图不存在/越权、非法转换、时间倒退。
    async fn advance_delete(
        &mut self,
        id: IntentId,
        next: PutPhase,
        at: UtcTimestamp,
    ) -> Result<DeleteIntent, IntentLogError>;

    /// 按身份读一条删除意图；不存在或越权返回 `None`。
    ///
    /// # Errors
    ///
    /// 存储失败或行无法还原。
    async fn load_delete(&mut self, id: IntentId) -> Result<Option<DeleteIntent>, IntentLogError>;

    /// 按 Root 列出尚未收尾的删除意图（`prepared`/`published`/`needs_recovery`），按写入顺序。
    ///
    /// # Errors
    ///
    /// 见 [`IntentLogError`]：越权（不能返回空列表冒充“没有未完成意图”）、存储失败。
    async fn unfinished_deletes(
        &mut self,
        root: RootId,
    ) -> Result<Vec<DeleteIntent>, IntentLogError>;
}
