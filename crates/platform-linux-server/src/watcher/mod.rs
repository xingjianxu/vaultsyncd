//! inotify 目录监视（design §5.4 的 `watcher/`、§12.1）。
//!
//! 只产出变化线索（[`WatchSignal`]），不判断“谁写的”、不推导删除；溢出与登记缺口显式上报。

pub mod inotify;
mod raw;

pub use inotify::{DirWatcher, GapReason, WatchConfig, WatchError, WatchSignal};
