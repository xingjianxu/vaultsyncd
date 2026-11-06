//! 发布之前的业务恢复上下文；意图绑定与 fs_intent 创建使用同一事务。
use vaultsyncd_domain::ids::{IntentId, UserId};
use vaultsyncd_server_store::{db::Database, operations::OperationKey};

/// 一个逻辑操作的持久恢复上下文。kind 区分元数据事务，context 不依赖内存或日志。
#[derive(Clone, Debug)]
pub struct Binding {
    /// Root。
    pub root: vaultsyncd_domain::ids::RootId,
    /// 设备。
    pub device: vaultsyncd_domain::ids::DeviceId,
    /// 操作。
    pub op: vaultsyncd_domain::ids::OpId,
    /// 元数据事务类型。
    pub kind: String,
    /// 类型特有事实（版本恢复来源、冲突副本身份、完整恢复清单等）。
    pub context: serde_json::Value,
    /// 上下文已经由更早的事务持久化（整组恢复计划）；只关联新意图，不再写上下文。
    linked: bool,
}
impl Binding {
    /// 建立上下文，操作键从已授权请求传入。
    pub fn new(key: OperationKey, kind: &str, context: serde_json::Value) -> Self {
        Self {
            root: key.root,
            device: key.device,
            op: key.op,
            kind: kind.to_owned(),
            context,
            linked: false,
        }
    }
    /// 只关联：操作的恢复上下文已先于任何意图落盘（回收站整组恢复的计划），新意图仅与它挂钩。
    pub fn linked(key: OperationKey) -> Self {
        Self {
            linked: true,
            ..Self::new(key, "", serde_json::Value::Null)
        }
    }
    /// 对应的操作键。
    pub fn key(&self) -> OperationKey {
        OperationKey {
            root: self.root,
            device: self.device,
            op: self.op,
        }
    }

    /// 只允许首次写入或逐字相同的重入，绝不改写已有恢复计划。
    pub async fn persist(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<(), sqlx::Error> {
        let text = self.context.to_string();
        sqlx::query("INSERT INTO execution_journal(root_id,device_id,op_id,kind,context_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING")
            .bind(self.root.as_bytes().as_slice()).bind(self.device.as_bytes().as_slice())
            .bind(self.op.as_bytes().as_slice()).bind(&self.kind).bind(&text).execute(&mut **tx).await?;
        let (kind, stored): (String, String) = sqlx::query_as("SELECT kind,context_json FROM execution_journal WHERE root_id=?1 AND device_id=?2 AND op_id=?3")
            .bind(self.root.as_bytes().as_slice()).bind(self.device.as_bytes().as_slice()).bind(self.op.as_bytes().as_slice())
            .fetch_one(&mut **tx).await?;
        if kind != self.kind || stored != text {
            return Err(sqlx::Error::Protocol(
                "同一操作的恢复上下文不一致".to_owned(),
            ));
        }
        Ok(())
    }
    /// 与意图在同一事务内关联，崩溃不会只留下一半关联。
    pub async fn attach(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        intent: IntentId,
    ) -> Result<(), sqlx::Error> {
        if !self.linked {
            self.persist(tx).await?;
        }
        sqlx::query(
            "INSERT INTO intent_operations(intent_id,root_id,device_id,op_id) VALUES (?1,?2,?3,?4)",
        )
        .bind(intent.as_bytes().as_slice())
        .bind(self.root.as_bytes().as_slice())
        .bind(self.device.as_bytes().as_slice())
        .bind(self.op.as_bytes().as_slice())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

type JournalRow = (Vec<u8>, Vec<u8>, Vec<u8>, String, String);

/// 读取意图的唯一业务上下文；未关联的旧意图返回 None，调用方须保持停写。
pub async fn for_intent(
    db: &Database,
    owner: UserId,
    intent: IntentId,
) -> Result<Option<Binding>, sqlx::Error> {
    let row: Option<JournalRow> = sqlx::query_as(
        "SELECT j.root_id,j.device_id,j.op_id,j.kind,j.context_json FROM intent_operations i JOIN execution_journal j USING(root_id,device_id,op_id) JOIN roots r ON r.id=j.root_id WHERE i.intent_id=?1 AND r.owner_id=?2")
        .bind(intent.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_optional(db.pool()).await?;
    row.map(|(root, device, op, kind, text)| {
        let bytes = |v: Vec<u8>| {
            v.try_into()
                .map_err(|_| sqlx::Error::Protocol("恢复身份长度错误".to_owned()))
        };
        Ok(Binding {
            root: vaultsyncd_domain::ids::RootId::from_bytes(bytes(root)?),
            device: vaultsyncd_domain::ids::DeviceId::from_bytes(bytes(device)?),
            op: vaultsyncd_domain::ids::OpId::from_bytes(bytes(op)?),
            kind,
            context: serde_json::from_str(&text)
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
            linked: false,
        })
    })
    .transpose()
}

/// 在结果 JSON 里写入客户端本地代际回显：重放、状态查询与重启补完都从同一份持久结果回显。
pub fn with_echo(
    result: String,
    generation: Option<vaultsyncd_domain::version::Generation>,
) -> String {
    let Some(generation) = generation else {
        return result;
    };
    match serde_json::from_str::<serde_json::Value>(&result) {
        Ok(serde_json::Value::Object(mut object)) => {
            object.insert(
                "echo_local_generation".to_owned(),
                serde_json::Value::String(generation.to_string()),
            );
            serde_json::Value::Object(object).to_string()
        }
        _ => result,
    }
}

/// 从恢复上下文取回本地代际。
pub fn echo_of(
    context: &serde_json::Value,
) -> Result<Option<vaultsyncd_domain::version::Generation>, String> {
    match context.get("local_generation") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .and_then(|text| text.parse().ok())
            .map(Some)
            .ok_or_else(|| "恢复上下文的 local_generation 不合法".to_owned()),
    }
}

/// 命令携带的本地代际 → 上下文字段。
pub fn echo_field(generation: Option<vaultsyncd_domain::version::Generation>) -> serde_json::Value {
    serde_json::json!({ "local_generation": generation.map(|value| value.to_string()) })
}

/// 提交事务内再核对一次：要提交结果的操作行仍是同一个请求摘要，且属于该账号的 Root。
/// 不一致说明有人拿别的请求的上下文去提交这条操作，整个事务回滚。
pub async fn ensure_digest(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: UserId,
    key: &OperationKey,
    expected: &vaultsyncd_server_store::operations::RequestDigest,
) -> Result<(), sqlx::Error> {
    let record = vaultsyncd_server_store::operations::operation_in_scope(tx, owner, key)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?
        .ok_or_else(|| sqlx::Error::Protocol("提交的操作不在该账号范围内".to_owned()))?;
    if record.digest.as_bytes() != expected.as_bytes() {
        return Err(sqlx::Error::Protocol(
            "操作摘要与持久记录不一致，拒绝提交".to_owned(),
        ));
    }
    Ok(())
}
