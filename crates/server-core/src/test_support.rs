//! 服务端核心测试共享的假端口与身份构造（仅 `cfg(test)` 编译）。
//!
//! 假端口只回答“被问了什么、按剧本答什么”，业务判定全部留给被测代码；调用顺序记录在
//! [`Trace`] 里，用来断言“先持久意图再发布”这类顺序不变量。

use std::cell::RefCell;
use std::rc::Rc;

use vaultsyncd_domain::ids::{EntryId, ID_LEN, IntentId, RootId};
use vaultsyncd_domain::version::UtcTimestamp;

use crate::commit::intent::{NewPutIntent, PutIntent, PutPhase, TargetSnapshot};
use crate::objects::ContentDigest;
use crate::ports::intent_log::{IntentLog, IntentLogError};
use crate::ports::object_store::FileKey;
use crate::ports::target::{
    PublishRequest, PublishedTarget, TargetError, TargetObservation, TargetPort,
};

/// 多个假端口共享的调用轨迹。
#[derive(Clone, Default)]
pub(crate) struct Trace(Rc<RefCell<Vec<String>>>);

impl Trace {
    pub(crate) fn record(&self, line: impl Into<String>) {
        self.0.borrow_mut().push(line.into());
    }

    pub(crate) fn entries(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

/// 共享同一条调用轨迹的意图日志与目标端口，用来断言跨端口的顺序。
pub(crate) fn linked_ports() -> (FakeIntentLog, FakeTarget) {
    let trace = Trace::default();
    (
        FakeIntentLog {
            trace: trace.clone(),
            ..FakeIntentLog::default()
        },
        FakeTarget {
            trace,
            ..FakeTarget::absent()
        },
    )
}

fn identity(marker: u8) -> [u8; ID_LEN] {
    [marker; ID_LEN]
}

pub(crate) fn intent_id(marker: u8) -> IntentId {
    IntentId::from_bytes(identity(marker))
}

pub(crate) fn root_id(marker: u8) -> RootId {
    RootId::from_bytes(identity(marker))
}

pub(crate) fn entry_id(marker: u8) -> EntryId {
    EntryId::from_bytes(identity(marker))
}

pub(crate) fn user_time(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(seconds).expect("测试用时间合法")
}

/// 测试用现场快照：设备 7、inode 41、单链接、revision 7、条目 0x51。
pub(crate) fn target_snapshot(hex: &str, size: u64) -> TargetSnapshot {
    TargetSnapshot {
        entry: entry_id(0x51),
        revision: 7,
        digest: ContentDigest::from_hex(hex).expect("测试用摘要合法"),
        identity: FileKey {
            device: 7,
            inode: 41,
            links: 1,
            bytes: size,
        },
    }
}

/// 内存意图日志：把阶段推进规则写成断言可读的记录。
#[derive(Default)]
pub(crate) struct FakeIntentLog {
    pub(crate) intents: Vec<PutIntent>,
    pub(crate) trace: Trace,
    pub(crate) begin_error: Option<IntentLogError>,
    pub(crate) advance_error: Option<IntentLogError>,
}

impl IntentLog for FakeIntentLog {
    async fn begin_put(
        &mut self,
        new: &NewPutIntent,
        _at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        self.trace.record("intent begin prepared");
        if let Some(error) = self.begin_error.take() {
            return Err(error);
        }
        let intent = new.prepared();
        self.intents.push(intent.clone());
        Ok(intent)
    }

    async fn advance_put(
        &mut self,
        id: IntentId,
        next: PutPhase,
        _at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        if let Some(error) = self.advance_error.take() {
            return Err(error);
        }
        let found = self
            .intents
            .iter_mut()
            .find(|intent| intent.intent_id == id)
            .ok_or(IntentLogError::UnknownIntent { id })?;
        if !found.phase.allows_transition_to(next) {
            return Err(IntentLogError::IllegalTransition {
                from: found.phase,
                to: next,
            });
        }
        found.phase = next;
        self.trace
            .record(format!("intent advance {}", next.as_text()));
        Ok(found.clone())
    }

    async fn load_put(&mut self, id: IntentId) -> Result<Option<PutIntent>, IntentLogError> {
        Ok(self
            .intents
            .iter()
            .find(|intent| intent.intent_id == id)
            .cloned())
    }

    async fn unfinished_puts(&mut self, root: RootId) -> Result<Vec<PutIntent>, IntentLogError> {
        Ok(self
            .intents
            .iter()
            .filter(|intent| intent.root == root && intent.phase.is_open())
            .cloned()
            .collect())
    }
}

/// 内存目标端口：可脚本化观察错误、发布错误与“名义成功但内容不符”。
pub(crate) struct FakeTarget {
    pub(crate) current: TargetObservation,
    pub(crate) trace: Trace,
    pub(crate) publishes: Vec<PublishRequest>,
    pub(crate) observe_error: Option<TargetError>,
    pub(crate) publish_error: Option<TargetError>,
    pub(crate) publish_reports: Option<PublishedTarget>,
    next_inode: u64,
}

impl FakeTarget {
    pub(crate) fn absent() -> Self {
        FakeTarget {
            current: TargetObservation::Absent,
            trace: Trace::default(),
            publishes: Vec::new(),
            observe_error: None,
            publish_error: None,
            publish_reports: None,
            next_inode: 90,
        }
    }

    /// 目标当前是一个已存在文件（现场快照 `snapshot`）。
    pub(crate) fn present(snapshot: &TargetSnapshot) -> Self {
        let mut target = FakeTarget::absent();
        target.current = TargetObservation::Present {
            identity: snapshot.identity,
            digest: snapshot.digest,
        };
        target.next_inode = snapshot.identity.inode + 1;
        target
    }
}

impl TargetPort for FakeTarget {
    fn observe(&mut self, target_path: &str) -> Result<TargetObservation, TargetError> {
        self.trace.record(format!("target observe {target_path}"));
        match self.observe_error.take() {
            Some(error) => Err(error),
            None => Ok(self.current.clone()),
        }
    }

    fn publish(&mut self, request: &PublishRequest) -> Result<PublishedTarget, TargetError> {
        self.trace
            .record(format!("target publish {}", request.target_path));
        self.publishes.push(request.clone());
        if let Some(error) = self.publish_error.take() {
            return Err(error);
        }
        let published = match self.publish_reports.take() {
            Some(reported) => reported,
            None => {
                self.next_inode += 1;
                PublishedTarget {
                    identity: FileKey {
                        device: 7,
                        inode: self.next_inode,
                        links: 1,
                        bytes: request.size,
                    },
                    digest: request.digest,
                }
            }
        };
        self.current = TargetObservation::Present {
            identity: published.identity,
            digest: published.digest,
        };
        Ok(published)
    }
}
