//! 取消开关与有界并发驱动（design §13.3「不同文件有界并发」「取消保留 dirty 内容」）。
//!
//! 取消是**状态**而不是失败：取消后不再启动新的块，已经在磁盘上的块与进度照旧保留，下次运行从
//! 缺失块继续。因此取消检查放在“开始下一块之前”，而不是把半块字节当成错误丢掉。
//!
//! 有界并发只约束**同时进行**的块数：本文件里的驱动一次最多推进 `limit` 个 future，超过的部分
//! 排队等待，某个块完成后才启动下一块。它不引入运行时、不 spawn 任务，因此可以在单线程 executor
//! 里并发推进真实的异步 I/O，也便于用内存替身断言“同时在飞行的块数不超过上限”。

use core::future::Future;
use core::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

/// 可克隆的取消开关。
///
/// 克隆之间共享同一个状态：调用方取消一次，所有持有点都能看到。它只表达“是否已取消”，不阻塞、
/// 不排队，也不替调用方清理任何东西。
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// 新建未取消的开关。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取消（幂等）。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 是否已取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 有界并发的执行结果：已完成的块与（可选的）第一个失败。
#[derive(Debug)]
pub(crate) struct BoundedOutcome<T, E> {
    /// 已完成的块（完成顺序）。
    pub completed: Vec<(usize, T)>,
    /// 第一个失败（块序号与错误）；出现后不再启动新块。
    pub failure: Option<(usize, E)>,
}

/// 一次最多推进 `limit` 个 future，遇到第一个错误就停止启动新块。
///
/// 返回的顺序是**完成顺序**：调用方据此逐个落进度，断线后已完成的部分不会白丢。出现错误时尚未
/// 完成的 future 被丢弃（异步请求被取消），因此不会在失败后继续发送请求。
pub(crate) async fn bounded_join<T, E, F>(futures: Vec<F>, limit: usize) -> BoundedOutcome<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    let limit = limit.max(1);
    let mut pending: Vec<(usize, core::pin::Pin<Box<F>>)> = futures
        .into_iter()
        .enumerate()
        .map(|(index, future)| (index, Box::pin(future)))
        .collect();
    let mut completed = Vec::new();
    let mut failure = None;

    poll_fn(|context| {
        let mut position = 0;
        while position < pending.len().min(limit) {
            match pending[position].1.as_mut().poll(context) {
                Poll::Ready(Ok(value)) => {
                    let (index, _) = pending.remove(position);
                    completed.push((index, value));
                }
                Poll::Ready(Err(error)) => {
                    let (index, _) = pending.remove(position);
                    if failure.is_none() {
                        failure = Some((index, error));
                    }
                    // 失败后不再推进剩余的块：丢弃它们，避免继续向不可用的服务端发请求。
                    pending.clear();
                }
                Poll::Pending => position += 1,
            }
        }
        if pending.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    BoundedOutcome { completed, failure }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 记录“同时在飞行的数量”峰值的最小 future：第一次 poll 登记并挂起，第二次 poll 注销并完成。
    struct Probe {
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        fail: bool,
        started: bool,
    }

    impl Future for Probe {
        type Output = Result<usize, &'static str>;

        fn poll(
            mut self: core::pin::Pin<&mut Self>,
            context: &mut core::task::Context<'_>,
        ) -> Poll<Self::Output> {
            if self.started {
                let now = self.in_flight.fetch_sub(1, Ordering::SeqCst);
                return Poll::Ready(Ok(now));
            }
            self.started = true;
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);
            if self.fail {
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                return Poll::Ready(Err("boom"));
            }
            // 让出一次：驱动会去推进别的块，因此峰值能反映并发上限。
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }

    async fn probes(
        count: usize,
        limit: usize,
        fail_at: Option<usize>,
    ) -> (BoundedOutcome<usize, &'static str>, usize) {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let futures: Vec<Probe> = (0..count)
            .map(|index| Probe {
                in_flight: Arc::clone(&in_flight),
                peak: Arc::clone(&peak),
                fail: fail_at == Some(index),
                started: false,
            })
            .collect();
        let outcome = bounded_join(futures, limit).await;
        (outcome, peak.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn never_more_than_the_limit_in_flight() {
        let (outcome, peak) = probes(8, 3, None).await;
        assert_eq!(outcome.completed.len(), 8);
        assert!(outcome.failure.is_none());
        assert_eq!(peak, 3, "并发峰值必须等于上限");
        let mut indices: Vec<usize> = outcome.completed.iter().map(|(index, _)| *index).collect();
        indices.sort_unstable();
        assert_eq!(indices, (0..8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_failure_stops_launching_new_blocks() {
        let (outcome, _) = probes(8, 2, Some(3)).await;
        assert_eq!(outcome.failure, Some((3, "boom")));
        assert!(outcome.completed.len() < 8, "失败后不再跑完全部块");
    }

    #[test]
    fn cancel_token_is_shared_between_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }
}
