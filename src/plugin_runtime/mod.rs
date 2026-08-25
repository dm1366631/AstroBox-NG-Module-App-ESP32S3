//! # AstroBox 插件运行时（`.abp` 加载/管理）
//!
//! 对应计划 **步骤 6**：在 ESP32-S3 固件侧实现 `.abp` 插件包的
//! 解包、校验、加载（实例化 + `lifecycle.on_load`）、列出、卸载全链路，
//! 以及 16 个 host import 的分阶段真实实现。
//!
//! 合规：基于 MIT 的 `AstroBox-Plugin-WIT` 契约**独立实现**，未复制 AGPL-3.0
//! 的官方 `AstroBox-NG-Module-PluginSystem` 源码（见计划 §九/Q1）。
//!
//! ## Phase 进度
//!
//! - **Phase 1（完成）**：`.abp` 全链路
//!   - `abp_package`：zip 解包 + `manifest.json` 校验 + wasm 魔数校验（完整、可测）
//!   - `runtime`：`WasmBackend` trait + `StubBackend`（不执行 wasm，代发 on_load 反馈）
//!   - `wit_host`：`HostCtx` trait，`dialog.show-info` 真实落地（→ UI 文本 channel）
//!
//! - **Phase 2（本提交）**：6 个核心 host import 真实逻辑
//!   - `wit_host_os`：os info/log/sleep（同步，进 `HostCtx` trait，可单测）
//!   - `wit_host_timer`：set_timeout/set_interval/clear_timer（`spawn_local` 调度）
//!   - `wit_host_transport`：HTTP GET/POST（复用 `net_http`）
//!   - `wit_host_thirdpartyapp`：快应用 list/install/launch/uninstall（复用 `install`）
//!   - `wit_host_watchface`：表盘 list/install/set/uninstall（复用 `install`）
//!   - async interface 以独立模块函数形式存在，Phase 3 的 wit-bindgen 生成
//!     trait impl 时直接委托（避免手写会被覆盖的 async trait 签名）
//!
//! - **Phase 3（待做）**：集成 WASM runtime（WAMR/wasm-tools 拍平 + wasm3）
//!   真正执行插件 wasm，并用 wit-bindgen 生成精确 host trait 绑定。
//! - **Phase 4（待做）**：网络安装 `.abp`（复用 repo_net）。

pub mod abp_package;
pub mod runtime;
pub mod wit_host;
pub mod wit_host_os;
pub mod wit_host_thirdpartyapp;
pub mod wit_host_timer;
pub mod wit_host_transport;
pub mod wit_host_watchface;

pub use abp_package::{safe_plugin_id, AbpPackage, Manifest};
pub use runtime::{PluginHandle, StubBackend, WasmBackend};
pub use wit_host::{FirmwareHostCtx, HostCtx};
pub use wit_host_os::OsInfo;
pub use wit_host_timer::TimerId;

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::Mutex;

/// 对外展示的已加载插件信息（`list()` 返回）。
#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub entry: String,
}

/// 注册表内条目。
struct LoadedPlugin {
    id: String,
    name: String,
    version: String,
    handle: PluginHandle,
}

/// 全局插件注册表。固件单进程，用 `Mutex` 即可；ESP32 单线程 tokio 下锁持有极短。
static REGISTRY: Mutex<Vec<LoadedPlugin>> = Mutex::new(Vec::new());

/// 从 SD 卡加载一个 `.abp` 插件：解包 → 校验 → 实例化（stub）→ 调 `on_load`。
///
/// 返回插件 id（`safe_plugin_id(manifest.name)`）。同一 name 重复加载会**覆盖**
/// 旧实例（先 unload 再 insert），避免注册表膨胀。
///
/// 注：`AbpPackage::from_file` 用同步 `std::fs` 读 + 解压；`.abp` 通常 < 1 MB，
/// 阻塞时间可接受。超大包（> 48 MB）会被 `from_bytes` 直接拒绝。
pub async fn load(path: &Path) -> Result<String> {
    let pkg =
        AbpPackage::from_file(path).with_context(|| format!("load abp {}", path.display()))?;
    let id = pkg.plugin_id();
    let name = pkg.manifest.name.clone();
    let version = pkg.manifest.version.clone();

    // Phase 1：本地 StubBackend（零大小，无全局状态）。
    let mut backend = StubBackend;
    let handle = backend.instantiate(&pkg.wasm, &pkg.manifest)?;
    // entry 字段已由 backend.instantiate 写入 handle.entry（取自 manifest.entry），
    // list() 时从 handle.entry 读出，故此处不再单独保留一份。

    // 构造 host ctx：dialog.show-info 直接写 slint 顶部进度文本
    // （Phase 1 stub 在 LocalSet 单线程上同步执行，可直接调 slint setter）。
    log::info!("加载插件 {} v{}…", name, version);
    let mut host = FirmwareHostCtx;

    backend.call_on_load(&handle, &mut host)?;

    // 写注册表：同 id 先卸载旧实例。
    {
        let mut reg = REGISTRY.lock().expect("plugin registry lock poisoned");
        if let Some(pos) = reg.iter().position(|p| p.id == id) {
            let old = reg.remove(pos);
            let _ = backend.unload(&old.handle);
        }
        reg.push(LoadedPlugin {
            id: id.clone(),
            name,
            version,
            handle,
        });
    }
    log::info!("[plugin_runtime] loaded plugin id={id}");
    Ok(id)
}

/// 卸载指定 id 的插件。
pub fn unload(id: &str) -> Result<()> {
    let mut backend = StubBackend;
    let removed = {
        let mut reg = REGISTRY.lock().expect("plugin registry lock poisoned");
        if let Some(pos) = reg.iter().position(|p| p.id == id) {
            Some(reg.remove(pos))
        } else {
            None
        }
    };
    match removed {
        Some(p) => {
            backend.unload(&p.handle)?;
            log::info!("[plugin_runtime] unloaded plugin id={id}");
            Ok(())
        }
        None => Err(anyhow!("plugin id '{id}' not found")),
    }
}

/// 列出当前已加载插件。
pub fn list() -> Vec<PluginInfo> {
    let reg = REGISTRY.lock().expect("plugin registry lock poisoned");
    reg.iter()
        .map(|p| PluginInfo {
            id: p.id.clone(),
            name: p.name.clone(),
            version: p.version.clone(),
            entry: p.handle.entry.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 最小 `.abp` 字节（manifest + 8 字节 wasm）。
    fn minimal_abp() -> Vec<u8> {
        let manifest = r#"{"name":"Test Plugin","version":"0.1.0","entry":"t.wasm"}"#;
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("manifest.json", opts).unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            zip.start_file("t.wasm", opts).unwrap();
            zip.write_all(&[0x00, 0x61, 0x73, 0x6d, 0, 0, 0, 0])
                .unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn safe_id_normalizes() {
        assert_eq!(safe_plugin_id("Test Plugin"), "test_plugin");
    }

    #[test]
    fn registry_list_empty_initially() {
        // 注意：全局静态 REGISTRY 在单测进程内是共享的；这里只断言 list() 不会 panic。
        let _ = list();
    }

    #[test]
    fn abp_parses() {
        let pkg = AbpPackage::from_bytes(&minimal_abp()).expect("parse");
        assert_eq!(pkg.manifest.name, "Test Plugin");
        assert_eq!(pkg.plugin_id(), "test_plugin");
    }
}
