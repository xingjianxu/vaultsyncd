//! vaultsyncd 的三方比较与确定性同步计划（`docs/design.md` §4.3、§5.4、§9.1、§9.2、I02、I04、I07）。
//!
//! 职责边界：本 crate 只做纯规则——把共同基线 B、本地 L 与最新远端 R 的事实转换成一份可执行的
//! 计划（[`reconcile::Plan`]）。它不打开文件、不访问网络或数据库、不读时钟、不分配身份，也不
//! 直接覆盖或删除任何文件；同一输入必须得到同一计划，执行前的现场复核由执行器按计划中的前提
//! （[`reconcile::Prerequisite`]）与依赖（[`reconcile::Dependency`]）重新完成。
//!
//! 两个入口按“有没有共同基线”分开，不能互相替代：
//!
//! - [`reconcile::reconcile`]：有基线（或已同步过、只是游标过期的设备带旧基线重建）时的三方比较，
//!   落 design §9.1 的四条基本分支；没有基线时只返回 [`reconcile::DeferReason::FirstSync`] 的转交信号。
//! - [`first_sync::plan_first_sync`]：没有基线的首次合并，落 design §9.2 第 4—7 步；它不接受基线
//!   参数，因此不会把“另一端没有”当成删除（I07）。
//!
//! 冲突与结构变化各有专门入口，彼此不重叠：
//!
//! - [`conflict`]：design §10.1—10.2 的内容冲突（服务器 head 保留主路径 + 迟到分支的稳定身份冲突副本、
//!   按名称预算截断的冲突名）。
//! - [`rename`]：design §9.5、§10.1 移动行的同 Vault 移动（移动+修改、双移动、移动+修改的保守双副本）
//!   与跨 Vault 的“目标创建确认 + 源删除”两段任务。
//! - [`delete`]：design §10.1 删除行、§10.3 的删除与恢复冲突（删除与修改/移动并发、目录子树 token）。
//!
//! [`reconcile`] 遇到这些输入仍然只给 [`reconcile::DeferReason`] 转交信号（`MissingEntry`、
//! `LocationOrKindChanged`），由上层按缺失方向改调上面某个入口；规划器自身从不生成未经裁决的删除或覆盖。
#![forbid(unsafe_code)]

pub mod conflict;
pub mod delete;
pub mod first_sync;
pub mod reconcile;
pub mod rename;
