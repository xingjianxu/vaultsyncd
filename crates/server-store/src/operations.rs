//! 幂等操作记录（`operations` 表）：请求摘要、状态机与“同一事务写批次与结果”
//! （`docs/design.md` §7.2、§11.2、§9.4、I06）。
//!
//! 边界：本模块只读写 `operations`，并调用 [`crate::changes`] 追加事务批次；它不实现具体命令
//! （Put/Create/Move/Delete 的校验与规划属提交协调逻辑），不做 HTTP，不碰工作目录与对象存储。
//!
//! 不变量：
//!
//! - **同一 OpId 同一请求只产生一个业务结果**（design I06）：幂等键是
//!   `(vault_id, device_id, op_id)`；[`begin_operation`] 首次写入 `pending`，重复调用返回
//!   [`BeginOperation::Replayed`] 且携带已记录的结果，调用方不得重做。
//! - **同一 OpId 不同请求拒绝**：`request_digest` 不同就是 [`OperationError::DigestMismatch`]，
//!   永远不会用旧结果回答一个新请求，也不会覆盖旧记录。
//! - **摘要基于规范化请求语义**：[`request_digest`] 只接受语义字段（名、值），按字段名排序后以
//!   长度前缀编码再做 SHA-256。JSON 键顺序、空白、等价字段顺序都不参与，因此“同一请求的两种写法”
//!   摘要相同；字段值不同则摘要必然不同。调用方传语义值，不传原始请求字节。
//! - **pending 不是 committed**（design I01）：`pending` 行不携带结果、不占 VaultSeq；只有
//!   [`commit_operation`] 在同一个事务里追加事件批次并写入结果，两者一起提交或一起回滚。
//! - **终态稳定**：`committed` 与 `rejected` 都是终态，重复的 `commit`/`reject` 返回
//!   [`OperationError::AlreadyFinished`]；重试路径是 `begin_operation` 拿到原结果，而不是重做。
//! - **紧凑结果的保留规则**：只有终态保留 `result_json`，必须是 JSON object 且不超过
//!   [`MAX_RESULT_BYTES`]（不塞事件、条目清单或文件内容）；记录与结果保留到相应设备代际失效
//!   （design §7.2、§9.4），届时由 [`discard_finished_operations`] 删除该设备的终态记录。`pending`
//!   记录**不**随代际失效删除：删掉它会让同一个 `op_id` 被当成新命令重新执行（design §9.4），
//!   因此它只能由后续 `commit`/`reject` 收尾。

use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{DeviceId, ID_LEN, OpId, TxId, UserId, VaultId};
use vaultsyncd_domain::version::{UtcTimestamp, VaultSeq};

use crate::changes::{self, ChangeBatch, ChangeError};

/// 请求摘要的字节长度（SHA-256）。
pub const REQUEST_DIGEST_LEN: usize = 32;

/// 紧凑结果的最大字节数；超出即拒绝，避免把事件或清单塞进结果。
pub const MAX_RESULT_BYTES: usize = 4096;

/// 规范化请求语义的版本前缀。字段编码规则变化时必须换前缀，否则旧摘要会被误认成新语义。
const DIGEST_DOMAIN: &[u8] = b"vaultsyncd.operation.request.v1\0";

/// 规范化请求语义的摘要（`operations.request_digest`）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RequestDigest([u8; REQUEST_DIGEST_LEN]);

impl RequestDigest {
    /// 由原始摘要字节构造（核心与存储层之间传递同一份摘要时必须用它，而不是重算）。
    pub const fn from_bytes(bytes: [u8; REQUEST_DIGEST_LEN]) -> Self {
        Self(bytes)
    }

    /// 原始字节（数据库 BLOB）。
    pub const fn as_bytes(&self) -> &[u8; REQUEST_DIGEST_LEN] {
        &self.0
    }

    /// 规范文本形式（小写十六进制），只用于日志与错误信息。
    pub fn to_hex(self) -> String {
        let mut text = String::with_capacity(REQUEST_DIGEST_LEN * 2);
        for byte in self.0 {
            text.push(char::from_digit(u32::from(byte >> 4), 16).expect("半字节是十六进制数字"));
            text.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("半字节是十六进制数字"));
        }
        text
    }
}

impl std::fmt::Debug for RequestDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RequestDigest({})", self.to_hex())
    }
}

/// 幂等键：同一设备在同一 Vault 内的 `OpId`（design §7.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationKey {
    /// 操作所在的 Vault。
    pub vault: VaultId,
    /// 发起操作的设备。
    pub device: DeviceId,
    /// 客户端生成的逻辑操作身份。
    pub op: OpId,
}

/// 操作记录的状态（`operations.state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationState {
    /// 已接受、尚未产生业务结果；不占 VaultSeq，不对外显示为已完成（design I01）。
    Pending,
    /// 已提交：同一事务里写入了事件批次与结果。
    Committed,
    /// 已稳定拒绝：结果对同一 OpId 保持不变，但不推进 VaultSeq。
    Rejected,
}

impl OperationState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Rejected => "rejected",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认值。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "committed" => Some(Self::Committed),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }

    /// 是否已经是终态（结果稳定，不再变化）。
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Committed | Self::Rejected)
    }
}

/// 开始一个操作的输入。
///
/// 不变量：`key.vault` 必须属于调用方；`digest` 必须由 [`request_digest`] 从该请求的语义字段算出。
#[derive(Clone, Copy, Debug)]
pub struct NewOperation {
    /// 幂等键。
    pub key: OperationKey,
    /// 规范化请求语义的摘要。
    pub digest: RequestDigest,
}

/// 从数据库读出的一条 `operations` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRecord {
    /// 幂等键。
    pub key: OperationKey,
    /// 记录下来的请求摘要。
    pub digest: RequestDigest,
    /// 状态。
    pub state: OperationState,
    /// 紧凑结果；只有终态有值。
    pub result_json: Option<String>,
    /// committed 操作推进到的 VaultSeq；`pending`/`rejected` 为 `None`。
    pub vault_seq: Option<VaultSeq>,
    /// 工作目录发布意图记录的位置（design §11.2 步骤 5）。
    pub intent_path: Option<String>,
}

/// [`begin_operation`] 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeginOperation {
    /// 首次见到这个幂等键：已写入 `pending` 行，调用方继续执行操作。
    Started,
    /// 同一幂等键与同一请求已存在：调用方必须直接使用这里的状态与结果，不得重做。
    Replayed(OperationRecord),
}

/// 提交一个操作的结果：追加事件批次并写入终态，两者在调用方的同一事务里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedOperation {
    /// 幂等键。
    pub key: OperationKey,
    /// 本事务追加的批次（其 `seq` 就是结果的 `vault_seq`）。
    pub batch: ChangeBatch,
    /// 写入的紧凑结果。
    pub result_json: String,
}

/// 操作记录的读写失败。
#[derive(Debug)]
pub enum OperationError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Vault 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 幂等键不存在（或不在调用方的 owner scope 内）。
    UnknownOperation {
        /// 请求的幂等键。
        key: OperationKey,
    },
    /// 同一幂等键带着不同的请求摘要再次出现：拒绝，绝不复用旧结果。
    DigestMismatch {
        /// 请求的幂等键。
        key: OperationKey,
    },
    /// 操作已是终态，不能再改结果。
    AlreadyFinished {
        /// 请求的幂等键。
        key: OperationKey,
        /// 已经记录的状态。
        state: OperationState,
    },
    /// 规范化请求的字段名为空。
    EmptyRequestField,
    /// 规范化请求里同一个字段名出现两次（语义不明确，拒绝而不是任选一个）。
    DuplicateRequestField {
        /// 重复的字段名。
        name: String,
    },
    /// 结果不是紧凑结果：为空或超过 [`MAX_RESULT_BYTES`]。
    InvalidResult {
        /// 具体原因。
        reason: &'static str,
    },
    /// 规范化请求的字段名超出可编码范围。
    RequestFieldTooLong {
        /// 字段名的字节长度。
        name_len: usize,
    },
    /// 同一幂等键已被别的请求占用（绕过本模块写入的坏数据）。
    DuplicateOperation {
        /// 请求的幂等键。
        key: OperationKey,
    },
    /// 意图路径为空。
    EmptyIntentPath,
    /// 事件批次写入失败。
    Change(ChangeError),
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for OperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Vault 不存在或不在调用方的 owner scope 内"),
            Self::UnknownOperation { key } => write!(f, "操作 {} 不存在", key.op),
            Self::DigestMismatch { key } => {
                write!(f, "操作 {} 已用不同的请求摘要登记过", key.op)
            }
            Self::AlreadyFinished { key, state } => write!(
                f,
                "操作 {} 已是终态 {}，结果不再改变",
                key.op,
                state.as_text()
            ),
            Self::EmptyRequestField => f.write_str("规范化请求的字段名不能为空"),
            Self::DuplicateRequestField { name } => {
                write!(f, "规范化请求的字段 {name:?} 出现了两次")
            }
            Self::InvalidResult { reason } => write!(f, "操作结果不合格：{reason}"),
            Self::RequestFieldTooLong { name_len } => {
                write!(f, "规范化请求的字段名有 {name_len} 字节，超出可编码范围")
            }
            Self::DuplicateOperation { key } => {
                write!(f, "操作 {} 的幂等键已被其它请求占用", key.op)
            }
            Self::EmptyIntentPath => f.write_str("意图路径不能为空"),
            Self::Change(source) => write!(f, "事件批次写入失败：{source}"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for OperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::Change(source) => Some(source),
            _ => None,
        }
    }
}

/// `operations` 表的列顺序，读取点共用。
const OPERATION_COLUMNS: &str = "vault_id, device_id, op_id, request_digest, state, result_json, \
                                 vault_seq, intent_path";

/// 由规范化请求语义算摘要。
///
/// 编码规则（顺序无关、无拼接歧义）：
///
/// 1. 丢弃空字段名、拒绝重复字段名；
/// 2. 按字段名升序排序；
/// 3. 逐字段写入 `u32` 大端名字长度 + 名字字节 + `u64` 大端值长度 + 值字节，前面加
///    固定的语义版本前缀；
/// 4. 对整段字节做 SHA-256。
///
/// 调用方给的是语义值：计数传规范十进制文本（design §15.1），身份传原始字节；**不要**传原始请求
/// 字节或 JSON 文本，否则键顺序和空白会进入摘要。
pub fn request_digest(fields: &[(&str, &[u8])]) -> Result<RequestDigest, OperationError> {
    let mut sorted = fields.to_vec();
    sorted.sort_by(|left, right| left.0.cmp(right.0));
    for (name, _) in &sorted {
        if name.is_empty() {
            return Err(OperationError::EmptyRequestField);
        }
    }
    for pair in sorted.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(OperationError::DuplicateRequestField {
                name: pair[0].0.to_owned(),
            });
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    for (name, value) in sorted {
        let name_len =
            u32::try_from(name.len()).map_err(|_| OperationError::RequestFieldTooLong {
                name_len: name.len(),
            })?;
        hasher.update(name_len.to_be_bytes());
        hasher.update(name.as_bytes());
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    let digest: [u8; REQUEST_DIGEST_LEN] = hasher.finalize().into();
    Ok(RequestDigest(digest))
}

/// 开始一个操作：首次写入 `pending`，重复的同一请求返回已记录状态。
///
/// 返回 [`BeginOperation::Replayed`] 时调用方不得再执行操作体，直接用记录里的结果回应客户端。
pub async fn begin_operation(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewOperation,
) -> Result<BeginOperation, OperationError> {
    require_vault_owner(tx, owner, new.key.vault).await?;
    if let Some(record) = operation_in_scope(tx, owner, &new.key).await? {
        if record.digest != new.digest {
            return Err(OperationError::DigestMismatch { key: new.key });
        }
        return Ok(BeginOperation::Replayed(record));
    }

    let inserted = sqlx::query(
        "INSERT INTO operations \
         (vault_id, device_id, op_id, request_digest, state, result_json, vault_seq, intent_path) \
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
    )
    .bind(new.key.vault.as_bytes().as_slice())
    .bind(new.key.device.as_bytes().as_slice())
    .bind(new.key.op.as_bytes().as_slice())
    .bind(new.digest.as_bytes().as_slice())
    .bind(OperationState::Pending.as_text())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_insert_error(error, new.key));
    }
    Ok(BeginOperation::Started)
}

/// 记下工作目录发布意图的位置（design §11.2 步骤 5）；只有 `pending` 操作可以设置。
pub async fn attach_intent(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    key: &OperationKey,
    intent_path: &str,
) -> Result<(), OperationError> {
    if intent_path.is_empty() {
        return Err(OperationError::EmptyIntentPath);
    }
    let record = require_operation(tx, owner, key).await?;
    if record.state.is_finished() {
        return Err(OperationError::AlreadyFinished {
            key: *key,
            state: record.state,
        });
    }
    sqlx::query(
        "UPDATE operations SET intent_path = ?1 \
         WHERE vault_id = ?2 AND device_id = ?3 AND op_id = ?4 AND state = 'pending'",
    )
    .bind(intent_path)
    .bind(key.vault.as_bytes().as_slice())
    .bind(key.device.as_bytes().as_slice())
    .bind(key.op.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(OperationError::Db)?;
    Ok(())
}

/// 提交操作结果：在调用方的事务里追加事件批次，并把操作写成 `committed`。
///
/// 这是 design §11.2 步骤 7 的“元数据完成事务”：事件批次、`vaults.seq` 推进与操作结果同一个提交，
/// 因此客户端重试拿到的是同一个 `vault_seq` 与同一份结果，不存在“结果写了但事件没写”。
pub async fn commit_operation(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    key: &OperationKey,
    tx_id: TxId,
    events_json: &str,
    result_json: &str,
    at: UtcTimestamp,
) -> Result<CommittedOperation, OperationError> {
    require_result(tx, result_json).await?;
    let record = require_operation(tx, owner, key).await?;
    if record.state.is_finished() {
        return Err(OperationError::AlreadyFinished {
            key: *key,
            state: record.state,
        });
    }

    let batch = changes::append_change(tx, owner, key.vault, tx_id, events_json, at)
        .await
        .map_err(OperationError::Change)?;

    let updated = sqlx::query(
        "UPDATE operations SET state = 'committed', result_json = ?1, vault_seq = ?2 \
         WHERE vault_id = ?3 AND device_id = ?4 AND op_id = ?5 AND state = 'pending'",
    )
    .bind(result_json)
    .bind(batch.seq.to_db_integer())
    .bind(key.vault.as_bytes().as_slice())
    .bind(key.device.as_bytes().as_slice())
    .bind(key.op.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(OperationError::Db)?;
    if updated.rows_affected() != 1 {
        return Err(OperationError::AlreadyFinished {
            key: *key,
            state: OperationState::Committed,
        });
    }

    crate::activity::committed(tx, key, events_json)
        .await
        .map_err(OperationError::Db)?;

    Ok(CommittedOperation {
        key: *key,
        batch,
        result_json: result_json.to_owned(),
    })
}

/// 稳定拒绝一个操作：写入终态 `rejected` 与结果，不推进 VaultSeq。
///
/// 用于无法自动解决、且对同一 OpId 必须给出同一答复的提交（design §11.2、§15.3）。
pub async fn reject_operation(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    key: &OperationKey,
    result_json: &str,
) -> Result<OperationRecord, OperationError> {
    require_result(tx, result_json).await?;
    let record = require_operation(tx, owner, key).await?;
    if record.state.is_finished() {
        return Err(OperationError::AlreadyFinished {
            key: *key,
            state: record.state,
        });
    }
    let updated = sqlx::query(
        "UPDATE operations SET state = 'rejected', result_json = ?1, vault_seq = NULL \
         WHERE vault_id = ?2 AND device_id = ?3 AND op_id = ?4 AND state = 'pending'",
    )
    .bind(result_json)
    .bind(key.vault.as_bytes().as_slice())
    .bind(key.device.as_bytes().as_slice())
    .bind(key.op.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(OperationError::Db)?;
    if updated.rows_affected() != 1 {
        return Err(OperationError::UnknownOperation { key: *key });
    }
    require_operation(tx, owner, key).await
}

/// 按幂等键读一条操作记录；不存在或不在 owner scope 内返回 `None`。
pub async fn operation_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    key: &OperationKey,
) -> Result<Option<OperationRecord>, OperationError> {
    let row = sqlx::query_as::<_, OperationRow>(&format!(
        "SELECT {OPERATION_COLUMNS} FROM operations o JOIN vaults r ON r.id = o.vault_id \
         WHERE o.vault_id = ?1 AND o.device_id = ?2 AND o.op_id = ?3 AND r.owner_id = ?4"
    ))
    .bind(key.vault.as_bytes().as_slice())
    .bind(key.device.as_bytes().as_slice())
    .bind(key.op.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(OperationError::Db)?;
    row.map(OperationRow::into_record).transpose()
}

/// 按 OpId 读一条操作记录（跨设备查询用）：同一 Vault 的同一 OpId 必须唯一。
///
/// 客户端的结果查询只知道自己的 `OpId`（`GET /v1/vaults/{vault}/operations/{op}`，design §15.2），
/// 幂等键里的设备由记录本身给出。`OpId` 是 128 位随机值，同一 Vault 内跨设备撞值意味着坏数据；
/// 发现多行时不猜哪一个，报 [`OperationError::DuplicateOperation`]。
pub async fn operation_of_vault_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
    op: OpId,
) -> Result<Option<OperationRecord>, OperationError> {
    let rows = sqlx::query_as::<_, OperationRow>(&format!(
        "SELECT {OPERATION_COLUMNS} FROM operations o JOIN vaults r ON r.id = o.vault_id \
         WHERE o.vault_id = ?1 AND o.op_id = ?2 AND r.owner_id = ?3"
    ))
    .bind(vault.as_bytes().as_slice())
    .bind(op.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(OperationError::Db)?;
    let mut records = rows
        .into_iter()
        .map(OperationRow::into_record)
        .collect::<Result<Vec<_>, _>>()?;
    match records.len() {
        0 => Ok(None),
        1 => Ok(records.pop()),
        _ => Err(OperationError::DuplicateOperation {
            key: OperationKey {
                vault,
                device: records[0].key.device,
                op,
            },
        }),
    }
}

/// 删除某设备在某 Vault 下的终态操作记录，返回删除行数。
///
/// 调用时机由设备代际/会话生命周期决定（design §7.2、§9.4：保留紧凑结果直到相应设备代际失效）。
/// `pending` 记录保留：删掉它会让同一个 `op_id` 被当作新命令重新执行。
pub async fn discard_finished_operations(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    vault: VaultId,
    device: DeviceId,
) -> Result<u64, OperationError> {
    require_vault_owner(tx, owner, vault).await?;
    let deleted = sqlx::query(
        "DELETE FROM operations \
         WHERE vault_id = ?1 AND device_id = ?2 AND state IN ('committed', 'rejected')",
    )
    .bind(vault.as_bytes().as_slice())
    .bind(device.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(OperationError::Db)?;
    Ok(deleted.rows_affected())
}

/// 确认 Vault 属于调用方的 owner scope。
async fn require_vault_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
) -> Result<(), OperationError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM vaults WHERE id = ?1")
        .bind(vault.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(OperationError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(OperationError::OutOfScope),
    }
}

/// 读一条必须存在的操作记录。
async fn require_operation(
    conn: &mut SqliteConnection,
    owner: UserId,
    key: &OperationKey,
) -> Result<OperationRecord, OperationError> {
    operation_in_scope(conn, owner, key)
        .await?
        .ok_or(OperationError::UnknownOperation { key: *key })
}

/// 紧凑结果检查：非空、不超过上限、并且是 JSON object。
///
/// JSON 形状用 SQLite 自带的 JSON 函数判定（与迁移里的 CHECK 同一套语义），因此“合法 JSON 但不是
/// object”与“根本不是 JSON”都得到同一个可区分的错误，而不是数据库约束错误。
async fn require_result(
    conn: &mut SqliteConnection,
    result_json: &str,
) -> Result<(), OperationError> {
    if result_json.is_empty() {
        return Err(OperationError::InvalidResult {
            reason: "结果为空"
        });
    }
    if result_json.len() > MAX_RESULT_BYTES {
        return Err(OperationError::InvalidResult {
            reason: "结果超过紧凑结果上限",
        });
    }
    // `json_type` 对非法 JSON 会报错，先用 `json_valid` 短接，把“不是 JSON”与“是 JSON 但不是 object”
    // 都归到同一个可区分的错误。
    let is_object: i64 = sqlx::query_scalar(
        "SELECT CASE WHEN json_valid(?1) THEN json_type(?1) = 'object' ELSE 0 END",
    )
    .bind(result_json)
    .fetch_one(conn)
    .await
    .map_err(OperationError::Db)?;
    if is_object != 1 {
        return Err(OperationError::InvalidResult {
            reason: "结果必须是 JSON object",
        });
    }
    Ok(())
}

/// 数据库行到 [`OperationRecord`] 的中间形态。
#[derive(sqlx::FromRow)]
struct OperationRow {
    vault_id: Vec<u8>,
    device_id: Vec<u8>,
    op_id: Vec<u8>,
    request_digest: Vec<u8>,
    state: String,
    result_json: Option<String>,
    vault_seq: Option<i64>,
    intent_path: Option<String>,
}

impl OperationRow {
    fn into_record(self) -> Result<OperationRecord, OperationError> {
        let digest: [u8; REQUEST_DIGEST_LEN] =
            self.request_digest.as_slice().try_into().map_err(|_| {
                OperationError::MalformedRow {
                    column: "request_digest",
                }
            })?;
        Ok(OperationRecord {
            key: OperationKey {
                vault: identity(self.vault_id, "vault_id", VaultId::from_bytes)?,
                device: identity(self.device_id, "device_id", DeviceId::from_bytes)?,
                op: identity(self.op_id, "op_id", OpId::from_bytes)?,
            },
            digest: RequestDigest(digest),
            state: OperationState::from_text(&self.state)
                .ok_or(OperationError::MalformedRow { column: "state" })?,
            result_json: self.result_json,
            vault_seq: self
                .vault_seq
                .map(|seq| {
                    VaultSeq::from_db_integer(seq).map_err(|_| OperationError::MalformedRow {
                        column: "vault_seq",
                    })
                })
                .transpose()?,
            intent_path: self.intent_path,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, OperationError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| OperationError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 把操作相关写语句的约束错误翻译成带上下文的类型化错误。
fn classify_insert_error(error: sqlx::Error, key: OperationKey) -> OperationError {
    let Some(database_error) = error.as_database_error() else {
        return OperationError::Db(error);
    };
    let message = database_error.message();
    if message.starts_with("UNIQUE constraint failed") {
        // 幂等键撞车：写事务是 `BEGIN IMMEDIATE` 串行的，即使并发也不会走到这里；出现即说明库里的
        // 记录被绕过本模块写过，按坏数据报错而不是覆盖别人的记录。
        return OperationError::DuplicateOperation { key };
    }
    if message.contains("result_json") {
        return OperationError::InvalidResult {
            reason: "结果必须是合法 JSON object",
        };
    }
    OperationError::Db(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::changes;
    use crate::db::Database;
    use crate::test_support::{
        TestDir, device, op, open_database, register_vault, seed_owner, tx as tx_id, user,
    };
    use crate::vaults;

    fn at(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("固定时间戳必须合法")
    }

    const EVENTS: &str = r#"[{"kind":"entry_created","entry":"01"}]"#;
    const RESULT: &str = r#"{"status":"committed","entry_id":"01","revision":"2"}"#;

    fn key(vault: VaultId, marker: u8) -> OperationKey {
        OperationKey {
            vault,
            device: device(0x71),
            op: op(marker),
        }
    }

    fn digest_of(name: &str) -> RequestDigest {
        request_digest(&[("kind", b"put_content"), ("name", name.as_bytes())]).expect("算摘要")
    }

    async fn vault_seq(database: &Database, owner: UserId, vault: VaultId) -> VaultSeq {
        let mut conn = database.pool().acquire().await.expect("取连接");
        vaults::vault_in_scope(&mut conn, owner, vault)
            .await
            .expect("读 Vault")
            .expect("Vault 必须存在")
            .seq
    }

    #[tokio::test]
    async fn a_repeated_request_gets_the_stored_result_instead_of_a_second_result() {
        let dir = TestDir::new("operations-replay");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let key = key(vault, 0x81);
        let request = NewOperation {
            key,
            digest: digest_of("note.txt"),
        };

        let mut tx = database.begin_write().await.expect("开始写事务");
        assert_eq!(
            begin_operation(&mut tx, owner, &request)
                .await
                .expect("开始操作"),
            BeginOperation::Started
        );
        attach_intent(&mut tx, owner, &key, "/srv/intents/op-81")
            .await
            .expect("记录意图路径");
        // pending 不携带结果，也不占 VaultSeq。
        let pending = operation_in_scope(&mut tx, owner, &key)
            .await
            .expect("读操作")
            .expect("pending 记录存在");
        assert_eq!(pending.state, OperationState::Pending);
        assert_eq!(pending.result_json, None);
        assert_eq!(pending.vault_seq, None);
        assert_eq!(pending.intent_path.as_deref(), Some("/srv/intents/op-81"));

        let committed = commit_operation(&mut tx, owner, &key, tx_id(0x31), EVENTS, RESULT, at(10))
            .await
            .expect("提交操作");
        tx.commit().await.expect("提交事务");
        assert_eq!(committed.batch.seq, VaultSeq::new(1).expect("seq"));
        assert_eq!(committed.result_json, RESULT);

        // 客户端重试：同一个 OpId、同一个请求，拿到同一份结果与同一个 vault_seq，不再执行操作体。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let replayed = begin_operation(&mut tx, owner, &request)
            .await
            .expect("重放");
        tx.commit().await.expect("提交事务");
        match replayed {
            BeginOperation::Replayed(record) => {
                assert_eq!(record.state, OperationState::Committed);
                assert_eq!(record.result_json.as_deref(), Some(RESULT));
                assert_eq!(record.vault_seq, Some(committed.batch.seq));
                assert_eq!(record.digest, request.digest);
                assert_eq!(record.intent_path.as_deref(), Some("/srv/intents/op-81"));
            }
            other => panic!("期望 Replayed，实际 {other:?}"),
        }

        // 终态稳定：再提交/再拒绝对同一 OpId 都是错误，而不是产生第二个批次。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = commit_operation(&mut tx, owner, &key, tx_id(0x32), EVENTS, RESULT, at(11))
            .await
            .expect_err("已提交的操作不能再提交");
        assert!(
            matches!(
                error,
                OperationError::AlreadyFinished {
                    state: OperationState::Committed,
                    ..
                }
            ),
            "实际错误 {error:?}"
        );
        let error = reject_operation(&mut tx, owner, &key, r#"{"status":"rejected"}"#)
            .await
            .expect_err("已提交的操作不能再拒绝");
        assert!(matches!(error, OperationError::AlreadyFinished { .. }));
        let error = attach_intent(&mut tx, owner, &key, "/srv/intents/late")
            .await
            .expect_err("终态操作不能再改意图");
        assert!(matches!(error, OperationError::AlreadyFinished { .. }));
        tx.rollback().await.expect("回滚");
        assert_eq!(
            vault_seq(&database, owner, vault).await,
            VaultSeq::new(1).expect("seq"),
            "重试不得再推进 VaultSeq"
        );
        database.close().await;
    }

    #[tokio::test]
    async fn the_same_op_id_with_a_different_request_is_refused() {
        let dir = TestDir::new("operations-digest");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let key = key(vault, 0x81);

        let mut tx = database.begin_write().await.expect("开始写事务");
        begin_operation(
            &mut tx,
            owner,
            &NewOperation {
                key,
                digest: digest_of("note.txt"),
            },
        )
        .await
        .expect("开始操作");
        tx.commit().await.expect("提交");

        // 同 ID、不同请求：拒绝，绝不用旧结果回答新请求，也不覆盖旧记录。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = begin_operation(
            &mut tx,
            owner,
            &NewOperation {
                key,
                digest: digest_of("other.txt"),
            },
        )
        .await
        .expect_err("不同请求摘要必须拒绝");
        assert!(
            matches!(error, OperationError::DigestMismatch { key: actual } if actual == key),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let record = operation_in_scope(&mut conn, owner, &key)
            .await
            .expect("读操作")
            .expect("记录仍在");
        assert_eq!(record.digest, digest_of("note.txt"), "旧记录不被覆盖");
        assert_eq!(record.state, OperationState::Pending);
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn an_unresolved_operation_keeps_pending_and_a_rejection_is_stable() {
        let dir = TestDir::new("operations-rejected");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let key = key(vault, 0x81);
        let request = NewOperation {
            key,
            digest: digest_of("note.txt"),
        };
        let refusal = r#"{"status":"rejected","reason":"baseline_changed"}"#;

        let mut tx = database.begin_write().await.expect("开始写事务");
        begin_operation(&mut tx, owner, &request)
            .await
            .expect("开始操作");
        let rejected = reject_operation(&mut tx, owner, &key, refusal)
            .await
            .expect("稳定拒绝");
        tx.commit().await.expect("提交");
        assert_eq!(rejected.state, OperationState::Rejected);
        assert_eq!(rejected.result_json.as_deref(), Some(refusal));
        assert_eq!(rejected.vault_seq, None, "拒绝不推进 VaultSeq");
        assert_eq!(
            vault_seq(&database, owner, vault).await,
            VaultSeq::ZERO,
            "拒绝的操作不占批次"
        );

        // 重试得到同一个拒绝结果。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let replayed = begin_operation(&mut tx, owner, &request)
            .await
            .expect("重放");
        let error = reject_operation(&mut tx, owner, &key, r#"{"status":"rejected"}"#)
            .await
            .expect_err("拒绝也是终态");
        let error_commit =
            commit_operation(&mut tx, owner, &key, tx_id(0x31), EVENTS, RESULT, at(10))
                .await
                .expect_err("已拒绝的操作不能再提交");
        tx.commit().await.expect("提交");
        assert!(
            matches!(replayed, BeginOperation::Replayed(ref record) if record.result_json.as_deref() == Some(refusal)),
            "实际结果 {replayed:?}"
        );
        assert!(matches!(error, OperationError::AlreadyFinished { .. }));
        assert!(matches!(
            error_commit,
            OperationError::AlreadyFinished { .. }
        ));
        database.close().await;
    }

    #[tokio::test]
    async fn committing_writes_batch_and_result_together_so_a_rollback_leaves_nothing() {
        let dir = TestDir::new("operations-atomic");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let key = key(vault, 0x81);
        let request = NewOperation {
            key,
            digest: digest_of("note.txt"),
        };

        let mut tx = database.begin_write().await.expect("开始写事务");
        begin_operation(&mut tx, owner, &request)
            .await
            .expect("开始操作");
        commit_operation(&mut tx, owner, &key, tx_id(0x31), EVENTS, RESULT, at(10))
            .await
            .expect("提交操作");
        tx.rollback().await.expect("回滚整个完成事务");

        // 事件、seq 与操作结果三者一起消失：没有“结果写了但事件没写”的中间状态。
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            operation_in_scope(&mut conn, owner, &key)
                .await
                .expect("读操作"),
            None
        );
        assert!(
            changes::changes_after(&mut conn, owner, vault, VaultSeq::ZERO, 10)
                .await
                .expect("拉取批次")
                .is_empty(),
            "回滚的事务不得暴露事件"
        );
        drop(conn);
        assert_eq!(vault_seq(&database, owner, vault).await, VaultSeq::ZERO);

        // 再次开始时是全新的操作，而不是“已提交但没结果”的重放。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let restarted = begin_operation(&mut tx, owner, &request)
            .await
            .expect("重新开始");
        tx.commit().await.expect("提交");
        assert_eq!(restarted, BeginOperation::Started);
        database.close().await;
    }

    #[tokio::test]
    async fn results_must_be_compact_objects_and_owner_scope_is_enforced() {
        let dir = TestDir::new("operations-result");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let key = key(vault, 0x81);
        let request = NewOperation {
            key,
            digest: digest_of("note.txt"),
        };

        // 别人的账号既不能开始也不能读。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = begin_operation(&mut tx, user(0x12), &request)
            .await
            .expect_err("别人的账号不能开始操作");
        assert!(
            matches!(error, OperationError::OutOfScope),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        let mut tx = database.begin_write().await.expect("开始写事务");
        begin_operation(&mut tx, owner, &request)
            .await
            .expect("开始操作");
        for (result, reason) in [
            ("", "结果为空"),
            ("[]", "结果必须是 JSON object"),
            (r#"{"status":"committed""#, "结果必须是 JSON object"),
        ] {
            let error = commit_operation(&mut tx, owner, &key, tx_id(0x31), EVENTS, result, at(10))
                .await
                .expect_err("不合格的结果必须拒绝");
            match error {
                OperationError::InvalidResult { reason: actual } => {
                    assert_eq!(actual, reason, "结果 {result:?}")
                }
                other => panic!("期望 InvalidResult，实际 {other:?}"),
            }
        }
        let oversized = format!(r#"{{"blob":"{}"}}"#, "x".repeat(MAX_RESULT_BYTES));
        let error = commit_operation(
            &mut tx,
            owner,
            &key,
            tx_id(0x31),
            EVENTS,
            &oversized,
            at(10),
        )
        .await
        .expect_err("超过上限的结果必须拒绝");
        assert!(
            matches!(error, OperationError::InvalidResult { reason } if reason.contains("上限")),
            "实际错误 {error:?}"
        );
        // 不合格的结果不得推进 seq，也不得改变状态。
        assert_eq!(
            vaults::vault_in_scope(&mut tx, owner, vault)
                .await
                .expect("读 Vault")
                .expect("Vault 存在")
                .seq,
            VaultSeq::ZERO
        );
        assert_eq!(
            operation_in_scope(&mut tx, owner, &key)
                .await
                .expect("读操作")
                .expect("记录存在")
                .state,
            OperationState::Pending
        );
        let error = attach_intent(&mut tx, owner, &key, "")
            .await
            .expect_err("空意图路径必须拒绝");
        assert!(matches!(error, OperationError::EmptyIntentPath));
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            operation_in_scope(&mut conn, user(0x12), &key)
                .await
                .expect("别人的账号读不到"),
            None
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn discarding_an_invalidated_device_generation_keeps_pending_records() {
        let dir = TestDir::new("operations-discard");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let committed_key = key(vault, 0x81);
        let rejected_key = key(vault, 0x82);
        let pending_key = key(vault, 0x83);

        let mut tx = database.begin_write().await.expect("开始写事务");
        for (key, digest) in [
            (committed_key, digest_of("a.txt")),
            (rejected_key, digest_of("b.txt")),
            (pending_key, digest_of("c.txt")),
        ] {
            begin_operation(&mut tx, owner, &NewOperation { key, digest })
                .await
                .expect("开始操作");
        }
        commit_operation(
            &mut tx,
            owner,
            &committed_key,
            tx_id(0x31),
            EVENTS,
            RESULT,
            at(10),
        )
        .await
        .expect("提交操作");
        reject_operation(&mut tx, owner, &rejected_key, r#"{"status":"rejected"}"#)
            .await
            .expect("拒绝操作");
        tx.commit().await.expect("提交");

        let mut tx = database.begin_write().await.expect("开始写事务");
        let removed = discard_finished_operations(&mut tx, owner, vault, device(0x71))
            .await
            .expect("回收终态记录");
        tx.commit().await.expect("提交");
        assert_eq!(removed, 2, "只删除终态记录");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            operation_in_scope(&mut conn, owner, &committed_key)
                .await
                .expect("读操作"),
            None
        );
        assert_eq!(
            operation_in_scope(&mut conn, owner, &rejected_key)
                .await
                .expect("读操作"),
            None
        );
        // pending 记录保留：删掉它会让同一个 op_id 被当成新命令重新执行（design §9.4）。
        assert_eq!(
            operation_in_scope(&mut conn, owner, &pending_key)
                .await
                .expect("读操作")
                .expect("pending 记录仍在")
                .state,
            OperationState::Pending
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn reading_by_op_id_alone_matches_the_device_scoped_lookup() {
        let dir = TestDir::new("operations-by-op");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let (other_vault, _other_entry) = register_vault(&database, owner, 0x22).await;
        let key = key(vault, 0x81);

        let mut tx = database.begin_write().await.expect("开始写事务");
        begin_operation(
            &mut tx,
            owner,
            &NewOperation {
                key,
                digest: digest_of("a.txt"),
            },
        )
        .await
        .expect("开始操作");
        commit_operation(&mut tx, owner, &key, tx_id(0x31), EVENTS, RESULT, at(10))
            .await
            .expect("提交操作");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let by_op = operation_of_vault_in_scope(&mut conn, owner, vault, key.op)
            .await
            .expect("按 OpId 读操作")
            .expect("记录必须存在");
        assert_eq!(by_op, {
            operation_in_scope(&mut conn, owner, &key)
                .await
                .expect("读操作")
                .expect("记录必须存在")
        });
        assert_eq!(by_op.state, OperationState::Committed);
        assert_eq!(by_op.vault_seq, Some(VaultSeq::new(1).expect("seq")));

        // 同一个 OpId 在别的 Vault、或对别的 owner 不可见。
        assert_eq!(
            operation_of_vault_in_scope(&mut conn, owner, other_vault, key.op)
                .await
                .expect("读操作"),
            None
        );
        assert_eq!(
            operation_of_vault_in_scope(&mut conn, user(0x12), vault, key.op)
                .await
                .expect("读操作"),
            None
        );
        drop(conn);
        database.close().await;
    }

    #[test]
    fn the_digest_is_order_independent_and_covers_the_semantics() {
        let ordered = request_digest(&[
            ("kind", b"put_content"),
            ("name", "note.txt".as_bytes()),
            ("base_revision", b"17"),
        ])
        .expect("算摘要");
        let reordered = request_digest(&[
            ("base_revision", b"17"),
            ("name", "note.txt".as_bytes()),
            ("kind", b"put_content"),
        ])
        .expect("算摘要");
        assert_eq!(ordered, reordered, "字段顺序不进入摘要");

        let other_name = request_digest(&[
            ("kind", b"put_content"),
            ("name", "other.txt".as_bytes()),
            ("base_revision", b"17"),
        ])
        .expect("算摘要");
        assert_ne!(ordered, other_name, "语义不同则摘要不同");
        let missing =
            request_digest(&[("kind", b"put_content"), ("name", b"note.txt")]).expect("算摘要");
        assert_ne!(ordered, missing, "少一个字段就是另一个请求");
        assert_eq!(ordered.to_hex().len(), REQUEST_DIGEST_LEN * 2);

        // 字段名重复或为空：语义不明确，拒绝而不是任选一个。
        let error =
            request_digest(&[("kind", b"a"), ("kind", b"b")]).expect_err("重复字段名必须拒绝");
        assert!(
            matches!(error, OperationError::DuplicateRequestField { ref name } if name == "kind"),
            "实际错误 {error:?}"
        );
        let error = request_digest(&[("", b"a")]).expect_err("空字段名必须拒绝");
        assert!(matches!(error, OperationError::EmptyRequestField));
    }
}
