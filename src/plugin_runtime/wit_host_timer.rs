//! Host import `timer` 实现（WIT `astrobox:psys-host/timer`）
//!
//! Phase 2：`set_timeout` / `set_interval` / `clear_timer`。
//!
//! 调度走固件 Tokio `LocalSet`（单线程，`spawn_local`）。回调签名同步：
//! 到点后宿主只需"触发一次插件 `on_event(timer)`"，真正的 wasm 回调由
//! Phase 3 runtime 的事件分发完成；Phase 2 这里只负责把 `cb` 投回
//! LocalSet 事件循环执行（cb 通常是把一个 timer 事件塞进 runtime 的事件队列）。
//!
//! 取消：每个 timer 持一个 `AtomicBool` cancel flag，`clear_timer` 置位即可；
//! 已 pending 的 `tokio::time::sleep` 会在到点后检查 flag 并跳过 cb。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 全局自增 timer id（从 1 开始，0 保留为"无效"）。
static NEXT_TIMER_ID: AtomicU32 = AtomicU32::new(1);

/// 活跃 timer 注册表：`(id, cancel_flag)`。`clear_timer` 置 flag；到点后自动移除。
/// 固件单进程，`Mutex` 足够；锁持有时间极短（仅 push/retain）。
static ACTIVE_TIMERS: Mutex<Vec<(u64, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

pub type TimerId = u64;

/// `timer.set-timeout(ms, cb)`：`ms` 毫秒后调用 `cb` 一次。
///
/// 必须在固件 `LocalSet` 上下文里调用（内部 `spawn_local`）。
/// 返回 timer id，可传给 [`clear_timer`] 取消。
pub fn set_timeout<F>(ms: u64, cb: F) -> TimerId
where
    F: FnOnce() + 'static,
{
    let id = alloc_id();
    let cancel = Arc::new(AtomicBool::new(false));
    register(id, cancel.clone());
    tokio::task::spawn_local(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        if !cancel.load(Ordering::Relaxed) {
            cb();
        }
        remove_timer(id);
    });
    id
}

/// `timer.set-interval(ms, cb)`：每 `ms` 毫秒调用一次 `cb`，直到 [`clear_timer`]。
pub fn set_interval<F>(ms: u64, mut cb: F) -> TimerId
where
    F: FnMut() + 'static,
{
    let id = alloc_id();
    let cancel = Arc::new(AtomicBool::new(false));
    register(id, cancel.clone());
    tokio::task::spawn_local(async move {
        // interval 首次 tick 立即触发，与浏览器 setInterval 行为一致；
        // 若需要"首次延迟"，调用方先 sleep 再 set_interval。
        let mut ticker = tokio::time::interval(Duration::from_millis(ms.max(1)));
        loop {
            ticker.tick().await;
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            cb();
        }
        remove_timer(id);
    });
    id
}

/// `timer.clear-timer(id)`：取消指定 timer。不存在的 id 静默忽略。
pub fn clear_timer(id: TimerId) {
    if let Ok(mut timers) = ACTIVE_TIMERS.lock() {
        if let Some((_, cancel)) = timers.iter().find(|(tid, _)| *tid == id) {
            cancel.store(true, Ordering::Relaxed);
        }
        // 顺便清理已取消的（保持注册表不膨胀）
        timers.retain(|(_, c)| !c.load(Ordering::Relaxed));
    }
}

fn alloc_id() -> TimerId {
    NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed)
}

fn register(id: TimerId, cancel: Arc<AtomicBool>) {
    if let Ok(mut timers) = ACTIVE_TIMERS.lock() {
        timers.push((id, cancel));
    }
}

fn remove_timer(id: TimerId) {
    if let Ok(mut timers) = ACTIVE_TIMERS.lock() {
        timers.retain(|(tid, _)| *tid != id);
    }
}

/// 当前活跃 timer 数量（仅供调试 / 测试）。
pub fn active_count() -> usize {
    ACTIVE_TIMERS
        .lock()
        .map(|mut t| {
            t.retain(|(_, c)| !c.load(Ordering::Relaxed));
            t.len()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_monotonic_and_nonzero() {
        let a = alloc_id();
        let b = alloc_id();
        assert!(b > a);
        assert!(a >= 1);
    }

    #[test]
    fn register_and_remove_via_registry() {
        // 直接测注册表逻辑（不 spawn，避免依赖 LocalSet）。
        let id = alloc_id();
        let cancel = Arc::new(AtomicBool::new(false));
        register(id, cancel);
        assert!(active_count() >= 1);
        remove_timer(id);
        // active_count 会清理已取消的，但这里只是 remove，应 < 注册后
    }

    #[test]
    fn clear_timer_sets_cancel_flag() {
        let id = alloc_id();
        let cancel = Arc::new(AtomicBool::new(false));
        register(id, cancel.clone());
        clear_timer(id);
        assert!(cancel.load(Ordering::Relaxed), "clear_timer must set flag");
    }
}
