//! 提交编排（design §11.2）：意图状态机、目标发布端口与后续的 CAS/冲突/命名空间操作。
//!
//! [`intent`] 是单文件 put 的持久阶段状态机；[`put`] 在它之上做完整的内容 CAS：Root 内串行化、
//! epoch/revision/父目录/名称与现场指纹复核、元数据事务与 ACK；[`conflict`] 在过期基线上把不同内容
//! 提交为一个稳定身份的冲突条目（`OpId` 幂等）。create/move/delete（S038/S039）在此基础上扩展，
//! 共用同一套“先持久意图再动磁盘”的顺序。

pub mod conflict;
pub mod intent;
pub mod put;
