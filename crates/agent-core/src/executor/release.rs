//! 显式手动释放：实际内容核对、固定版本租约、连续使用保护、持久意图与条件驻留事务。
//!
//! 不扫描挑选缓存、不做 LRU。应用入口按绑定串行组装控制/内容动作；Windows 原生 guard
//! 由平台适配器实现（S069）。已释放的重启结果只靠平台固定身份与持久意图补 DB，不回源覆盖。
use std::sync::Arc;

use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::ports::{PortError, PortFuture, VersionPin};

use crate::bindings::control::{BindingControls, ExecutionGate};
use crate::executor::apply::ControlledLocal;
use crate::recovery::{self, ContentEvidence};

mod store;
use store::{Journal, Store};

/// 用户显式释放的固定目标；pin 来自服务器已提交版本事实，不能用本地猜测构造。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseSpec {
    /// 持久幂等身份；重放不能改变输入。
    pub op: OpId,
    /// 绑定身份。
    pub binding: BindingId,
    /// 服务器条目身份。
    pub entry: EntryId,
    /// 经纯路径规则核对的位置。
    pub path: RelativePath,
    /// 对应本地已确认字节的固定服务器版本。
    pub pin: VersionPin,
}

/// 服务端短内容租约；持有者在 Drop/显式释放前不得主动放弃租约。
/// 真实适配器应让服务器把此版本加入 GC 可达集，并在安全点续约。
pub trait VersionLease: Send + Sync {
    /// 服务器确认可读且受租约保护的完整版本事实。
    fn pin(&self) -> &VersionPin;
    /// 确认租约有效，必要时续约；过期/GC/断网均返回错误而不是默认有效。
    fn ensure_valid(&self) -> PortFuture<'_, Result<(), PortError>>;
}

/// 固定版本租约端口；acquire 必须原子核对可读事实并建立 GC 保护。
pub trait ReleaseRemote: Send + Sync {
    /// 返回的 lease 必须覆盖 pin 的条目/版本/摘要/大小，不能只保护“当前版本”。
    fn acquire<'a>(
        &'a self,
        pin: &'a VersionPin,
    ) -> PortFuture<'a, Result<Box<dyn VersionLease>, PortError>>;
}

/// 平台查询到的已释放结果；此证据不得通过读取文件内容触发 hydration。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasedFile {
    /// 释放前后相同的内容代际；脱水本身不是外部编辑。
    pub generation: u64,
    /// 稳定文件身份，不能仅凭路径或 clean 标记恢复。
    pub identity: Vec<u8>,
    /// 占位元数据中可信的固定内容身份、摘要与逻辑大小。
    pub pin: VersionPin,
}

/// 连续持有的使用保护；从 protect 成功到本对象释放，必须拒绝并发写、替换与删除。
/// 无法保护时 protect 返回 LOCAL_IN_USE/UNSUPPORTED，不能用短暂探测代替持续保护。
pub trait ReleaseProtection: Send + Sync {
    /// 同一受保护目标的 S059 真实内容观察端口；核心复用 inspect 流式校验。
    fn content(&self) -> &dyn ControlledLocal;
    /// 元数据查询是否已释放；未知、部分、身份不明要报错，不得当成完整或已释放。
    fn released(&self) -> PortFuture<'_, Result<Option<ReleasedFile>, PortError>>;
    /// 在持续保护内再次核对真实 expected 与租约有效性，再脱水并持久化。
    /// 不得在 lease 失效时清除内容；返回后仍持保护供核心验证与落库。
    fn dehydrate_verified<'a>(
        &'a self,
        expected: &'a ContentEvidence,
        lease: &'a dyn VersionLease,
    ) -> PortFuture<'a, Result<ReleasedFile, PortError>>;
}

/// 平台释放能力；不对缺乏安全使用保护的平台假装支持。
pub trait ReleaseLocal: Send + Sync {
    /// 获取特定路径的持续使用保护。保留未确认内容；不在这里释放任何字节。
    fn protect<'a>(
        &'a self,
        path: &'a RelativePath,
    ) -> PortFuture<'a, Result<Box<dyn ReleaseProtection>, PortError>>;
}

/// 唯一的释放入口，调用者必须来自显式用户请求；没有自动缓存淘汰入口。
pub struct ManualRelease {
    store: Store,
    controls: BindingControls,
    local: Arc<dyn ReleaseLocal>,
    remote: Arc<dyn ReleaseRemote>,
}
impl ManualRelease {
    /// Typed startup inputs from validated durable journals; applications must not parse private JSON.
    pub async fn pending_specs(
        database: Arc<Database>,
        binding: BindingId,
    ) -> Result<Vec<ReleaseSpec>, PortError> {
        Store { database }
            .pending(binding)
            .await?
            .iter()
            .map(|journal| journal.spec())
            .collect()
    }
    /// 组装客户端 SQLite、平台保护与服务器内容租约。
    pub fn new(
        database: Arc<Database>,
        local: Arc<dyn ReleaseLocal>,
        remote: Arc<dyn ReleaseRemote>,
    ) -> Self {
        Self {
            store: Store {
                database: database.clone(),
            },
            controls: BindingControls::new(database),
            local,
            remote,
        }
    }

    /// 执行或恢复同一显式请求。任何未知/变化/失败都保留字节或开放意图，不清 dirty。
    pub async fn release(&self, spec: &ReleaseSpec) -> Result<ReleasedFile, PortError> {
        validate(spec)?;
        recovery::resync::ensure_operation(&self.store.database, spec.binding, spec.op).await?;
        if let Some(journal) = self.store.load(spec.op).await? {
            if journal.spec()? != *spec {
                return Err(reject("OP_ID_REUSED", "释放请求的固定输入已改变"));
            }
            return self.resume(journal).await;
        }
        self.gate(spec).await?;
        let before = self.store.snapshot(spec).await?;
        let guard = self.local.protect(&spec.path).await?;
        if guard.released().await?.is_some() {
            return Err(reject(
                "UNKNOWN_LOCAL_STATE",
                "没有对应意图的已释放目标不能猜成成功",
            ));
        }
        let actual = recovery::inspect(guard.content(), &spec.path).await?;
        before.matches_content(&actual, &spec.pin)?;
        let lease = self.remote.acquire(&spec.pin).await?;
        check_lease(&*lease, &spec.pin).await?;
        self.gate(spec).await?;
        let journal = self.store.prepare(spec, &before, actual.clone()).await?;
        #[cfg(test)]
        tests::checkpoint("prepared", 111);
        self.perform(journal, guard, Some(lease)).await
    }

    /// 启动恢复开放释放意图；不把未知其它平台意图当成功。不扫描或自动挑选释放对象。
    pub async fn recover(
        &self,
        binding: BindingId,
    ) -> Result<Vec<(OpId, Result<ReleasedFile, PortError>)>, PortError> {
        recovery::resync::ensure_unblocked(&self.store.database, binding).await?;
        let mut results = vec![];
        for journal in self.store.pending(binding).await? {
            let op = journal.spec()?.op;
            results.push((op, self.resume(journal).await));
        }
        Ok(results)
    }

    async fn resume(&self, journal: Journal) -> Result<ReleasedFile, PortError> {
        let spec = journal.spec()?;
        self.gate(&spec).await?;
        self.store.check(&journal).await?;
        let guard = self.local.protect(&spec.path).await?;
        self.perform(journal, guard, None).await
    }

    async fn perform(
        &self,
        journal: Journal,
        guard: Box<dyn ReleaseProtection>,
        mut lease: Option<Box<dyn VersionLease>>,
    ) -> Result<ReleasedFile, PortError> {
        let spec = journal.spec()?;
        self.gate(&spec).await?;
        self.store.check(&journal).await?;
        let released = if let Some(released) = guard.released().await? {
            // Platform succeeded before crash, DB may still say materialized. Never read/hydrate it.
            journal.matches_released(&released)?;
            released
        } else {
            if journal.completed {
                return Err(recovery::changed());
            }
            if recovery::inspect(guard.content(), &spec.path).await? != journal.evidence {
                return Err(recovery::changed());
            }
            if lease.is_none() {
                lease = Some(self.remote.acquire(&spec.pin).await?);
            }
            let held = lease.as_deref().expect("租约已取得");
            check_lease(held, &spec.pin).await?;
            self.gate(&spec).await?;
            self.store.check(&journal).await?;
            // The adapter revalidates both evidence and the live lease under the same protection.
            let receipt = guard.dehydrate_verified(&journal.evidence, held).await?;
            journal.matches_released(&receipt)?;
            if guard.released().await?.as_ref() != Some(&receipt) {
                return Err(recovery::changed());
            }
            #[cfg(test)]
            tests::checkpoint("platform", 112);
            receipt
        };
        if !journal.completed {
            // Keep both platform protection and version lease through the SQLite CAS.
            self.store.finish(&journal, &released).await?;
            #[cfg(test)]
            tests::checkpoint("finished", 113);
        }
        drop(lease);
        drop(guard);
        Ok(released)
    }

    async fn gate(&self, spec: &ReleaseSpec) -> Result<(), PortError> {
        // Full has a precise product error even if its control gate would also refuse.
        self.store.require_on_demand(spec.binding).await?;
        match self
            .controls
            .execution_gate(spec.binding, Some(spec.entry))
            .await?
        {
            ExecutionGate::Ready => Ok(()),
            gate => Err(reject("BINDING_BLOCKED", &format!("绑定门禁：{gate:?}"))),
        }
    }
}
async fn check_lease(lease: &dyn VersionLease, pin: &VersionPin) -> Result<(), PortError> {
    if lease.pin() != pin {
        return Err(reject("VERSION_MISMATCH", "租约没有覆盖请求的固定版本"));
    }
    lease.ensure_valid().await
}
fn validate(spec: &ReleaseSpec) -> Result<(), PortError> {
    if spec.pin.binding != spec.binding.to_string()
        || spec.pin.entry != spec.entry.to_string()
        || vaultsyncd_domain::ids::VersionId::from_hex(&spec.pin.version).is_err()
    {
        return Err(reject("INVALID_ARGUMENT", "固定版本与绑定/条目不匹配"));
    }
    Ok(())
}
fn reject(code: &str, detail: &str) -> PortError {
    PortError::rejected(code, detail)
}
#[cfg(test)]
mod tests;
