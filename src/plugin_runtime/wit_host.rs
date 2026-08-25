//! # Host import 实现接入点（WIT `astrobox:psys-host`）
//!
//! 计划分阶段把 16 个 host interface 从 stub 替换为真实实现：
//!
//! | interface | Phase | 状态 |
//! |-----------|-------|------|
//! | `dialog.show-info` | 1 | ✅ 真实落地（→ UI 进度文本） |
//! | `os`（info/log/sleep） | 2 | ✅ 真实落地（→ [`wit_host_os`]） |
//! | `timer` | 2 | ✅ 逻辑实现（→ [`wit_host_timer`]，独立 async fn） |
//! | `transport`（HTTP） | 2 | ✅ 逻辑实现（→ [`wit_host_transport`]，独立 async fn） |
//! | `thirdpartyapp` | 2 | ✅ 逻辑实现（→ [`wit_host_thirdpartyapp`]，独立 async fn） |
//! | `watchface` | 2 | ✅ 逻辑实现（→ [`wit_host_watchface`]，独立 async fn） |
//! | 其余 9 个 | 3 | ⏳ 走 [`HostCtx::stub_log`]（`log::warn!` + 默认返回） |
//!
//! ## 为什么 async interface 不进 trait
//!
//! WIT 所有 host 函数返回 `future<T>`（WASI Component Model async）。trait 签名
//! 必须由 `wit-bindgen` 按**选定 runtime** 生成（`&mut self` → `future<T>`），
//! 手写一遍既无法编译验证又会在 Phase 3 被覆盖。故 `transport` / `timer` /
//! `thirdpartyapp` / `watchface` 的 async 实现以**独立模块函数**形式存在
//! （`wit_host_*.rs`），Phase 3 的 wit-bindgen 生成 trait impl 时直接委托给它们。
//!
//! 同步 interface（`os` / `dialog`）可直接进 [`HostCtx`] trait，Phase 1/2 即真实可用。
//!
//! ## 线程模型
//!
//! Phase 1 的 `StubBackend` 在固件 Tokio `LocalSet`（单线程）上同步调用
//! `call_on_load`，故 `dialog_show_info` 可直接调 slint setter。Phase 2 真正的
//! wasm 解释器可能跑在独立 native 线程，那时需把 UI 调用 marshal 回 LocalSet
//!（用 channel 或 `slint::invoke`）。

use crate::plugin_runtime::wit_host_os::OsInfo;

/// 插件 host 上下文：runtime 执行插件时把 host import 调用转发到这里。
///
/// 方法刻意做成**同步**（非 async）：wasm 解释器在某个 native 栈帧里同步回调
/// host import，不能再 `await`。async interface（transport/timer/...）见各
/// `wit_host_*.rs` 模块的独立 async 函数。
pub trait HostCtx {
    /// `dialog.show-info(content)`：把文本推到 UI 顶部进度条。Phase 1 核心落地。
    fn dialog_show_info(&mut self, content: &str);

    /// `os.get-*()`：返回 os 环境信息（arch/version/locale/timezone…）。
    /// Phase 2 真实落地，委托 [`wit_host_os::get_os_info`]。
    fn os_get_info(&mut self) -> OsInfo {
        crate::plugin_runtime::wit_host_os::get_os_info()
    }

    /// `os.log(level, msg)`：转发到固件 log crate。
    /// Phase 2 真实落地，委托 [`wit_host_os::log`]。
    fn os_log(&mut self, level: &str, msg: &str) {
        crate::plugin_runtime::wit_host_os::log(level, msg);
    }

    /// `os.sleep(ms)`：同步阻塞 sleep（仅 runtime native 线程可调）。
    /// Phase 2 真实落地，委托 [`wit_host_os::sleep_ms`]。
    fn os_sleep_ms(&mut self, ms: u64) {
        crate::plugin_runtime::wit_host_os::sleep_ms(ms);
    }

    /// 其余未实现 host import 的统一 stub 入口（debug 日志，便于真 runtime 接入后定位）。
    fn stub_log(&mut self, iface: &str, fn_name: &str);
}

/// 固件侧 `HostCtx` 实现：
/// - `dialog.show-info` → 直接写 slint 顶部进度文本
///   （[`gui::slint_ui::set_install_progress_text`]）
/// - `os.*` → 委托 [`wit_host_os`]
pub struct FirmwareHostCtx;

impl HostCtx for FirmwareHostCtx {
    fn dialog_show_info(&mut self, content: &str) {
        let line = format!("[plugin] {content}");
        log::info!("{line}");
        log::info!("[plugin_host] dialog.show-info: {line}");
    }

    fn stub_log(&mut self, iface: &str, fn_name: &str) {
        log::warn!(
            "[plugin_host] stub: {iface}.{fn_name} (not implemented in Phase 2; see wit_host_*.rs)"
        );
    }
}
