//! vaultsyncd 的三方比较与确定性同步计划（`docs/design.md` §4.3、§5.4、§9.1、I02、I04）。
//!
//! 职责边界：本 crate 只做纯规则——把共同基线 B、本地 L 与最新远端 R 的事实转换成一份可执行的
//! 计划（[`reconcile::Plan`]）。它不打开文件、不访问网络或数据库、不读时钟、不分配身份，也不
//! 直接覆盖或删除任何文件；同一输入必须得到同一计划，执行前的现场复核由执行器按计划中的前提
//! （[`reconcile::Prerequisite`]）与依赖（[`reconcile::Dependency`]）重新完成。
//!
//! 已落地模块（S019）：[`reconcile`] 的 design §9.1 四条基本分支（双方无变、仅本地变、仅远端变、
//! 双方已证明同一结果）及其动作前提、依赖与本地内容保护说明。首次合并、内容冲突副本、移动/删除
//! 矩阵分别由后续卡（`first_sync.rs`、`conflict.rs`、`rename.rs`/`delete.rs`）补齐；本卡遇到这些
//! 输入时只给出明确的延期裁决（[`reconcile::DeferReason`]），不生成删除或覆盖动作。
#![forbid(unsafe_code)]

pub mod reconcile;
