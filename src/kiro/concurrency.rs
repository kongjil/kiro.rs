//! 单账号并发占用
//!
//! 一次模型请求在选中账号时占用该账号的一个并发位，直到响应体读完（或请求被取消）
//! 才释放。占用以 [`ConcurrencyPermit`] 表示：随调用结果一路传递，最后一个持有者
//! 释放时计数减一，并唤醒正在等空位的请求。
//!
//! 只统计模型对话请求；MCP（联网搜索）等短请求不占并发位。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::Notify;

/// 一个账号的一个并发位。Drop 时归还。
#[derive(Debug)]
pub struct ConcurrencyPermit {
    in_flight: Arc<AtomicU32>,
    released: Arc<Notify>,
}

impl ConcurrencyPermit {
    /// 由调用方在确认未超限后创建；计数已在调用方加一。
    pub(crate) fn new(in_flight: Arc<AtomicU32>, released: Arc<Notify>) -> Self {
        Self {
            in_flight,
            released,
        }
    }
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        // 计数只会由持有 permit 的请求加一，这里不会下溢；saturating 只为防御。
        let _ = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(1)));
        self.released.notify_waiters();
    }
}

/// 在调用链上共享的占用句柄（`CallContext` 需要 `Clone`）。
pub type SharedPermit = Arc<ConcurrencyPermit>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_permit_releases_slot_and_wakes_waiters() {
        let in_flight = Arc::new(AtomicU32::new(1));
        let released = Arc::new(Notify::new());
        let permit = ConcurrencyPermit::new(in_flight.clone(), released.clone());

        let waiter = {
            let released = released.clone();
            tokio::spawn(async move { released.notified().await })
        };
        tokio::task::yield_now().await;

        drop(permit);
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter should be woken")
            .unwrap();
    }

    #[test]
    fn shared_permit_releases_once_after_last_clone() {
        let in_flight = Arc::new(AtomicU32::new(1));
        let permit: SharedPermit =
            Arc::new(ConcurrencyPermit::new(in_flight.clone(), Arc::new(Notify::new())));
        let clone = permit.clone();
        drop(permit);
        assert_eq!(in_flight.load(Ordering::Acquire), 1);
        drop(clone);
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
    }
}
