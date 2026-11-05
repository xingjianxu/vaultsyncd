//! 提交编排（design §11.2）：意图状态机、目标发布端口与后续的 CAS/冲突/命名空间操作。
//!
//! [`intent`] 是单文件 put 的持久阶段状态机；[`put`] 在它之上做完整的内容 CAS：Root 内串行化、
//! epoch/revision/父目录/名称与现场指纹复核、元数据事务与 ACK；[`conflict`] 在过期基线上把不同内容
//! 提交为一个稳定身份的冲突条目（`OpId` 幂等）；[`namespace`] 把目录创建与同 Root 移动接进同一套
//! “先持久意图再动磁盘”的顺序（`EntryId` 不变、移动不搬内容）；[`delete`] 把删除接进同一套阶段，
//! 用子树 token 证明前提、用有界批次移除工作目录内容，并在单一事务里公布回收站清单与墓碑。

pub mod conflict;
pub mod delete;
pub mod intent;
pub mod namespace;
pub mod put;
