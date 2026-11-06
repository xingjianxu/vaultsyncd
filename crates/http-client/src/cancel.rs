//! 请求取消：一个可克隆的“取消开关”，取消后不再等待传输。
//!
//! 取消只影响发起它的那次请求：已经发出的刷新请求不会被某个等待者中途丢弃，否则其他等待者
//! 会看到一个半途而废的刷新代际。取消不是错误分类中的“失败”，调用方应保留本地队列与未确认
//! 内容（design §13.2「取消」一行）。

use std::sync::Arc;

use tokio::sync::watch;

/// 可克隆的取消开关。
///
/// 克隆出来的是同一个开关；任意一个克隆调用 [`Cancellation::cancel`]，所有克隆上的
/// [`Cancellation::cancelled`] 都会完成。
#[derive(Clone, Debug)]
pub struct Cancellation {
    sender: Arc<watch::Sender<bool>>,
    receiver: watch::Receiver<bool>,
}

impl Cancellation {
    /// 新建一个未取消的开关。
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self {
            sender: Arc::new(sender),
            receiver,
        }
    }

    /// 取消。重复调用无副作用。
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }

    /// 是否已经取消。
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// 等待取消；已经取消时立即完成。
    pub async fn cancelled(&self) {
        let mut receiver = self.receiver.clone();
        if *receiver.borrow() {
            return;
        }
        // `wait_for` 只在值变为 true 时返回；发送端活着（本结构持有 sender），因此不会提前结束。
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_clone_cancels_the_shared_switch() {
        let cancellation = Cancellation::new();
        let clone = cancellation.clone();
        assert!(!cancellation.is_cancelled());
        clone.cancel();
        assert!(cancellation.is_cancelled());
        cancellation.cancelled().await;
    }

    #[tokio::test]
    async fn waiting_ends_when_the_switch_is_cancelled() {
        let cancellation = Cancellation::new();
        let waiter = cancellation.clone();
        let task = tokio::spawn(async move {
            waiter.cancelled().await;
            true
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        cancellation.cancel();
        assert!(task.await.expect("等待任务"));
    }
}
