//! 快应用/表盘包管理器（内存存储）。
//!
//! 提供安装、卸载、列表、查询功能。用 `Arc<Mutex<>>` 包装，
//! 可安全地在 HTTP 服务器线程中访问。

use crate::abp_package::{AbpPackage, PackageType};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 对外展示的已安装包信息。
#[derive(Clone, Debug, Serialize)]
pub struct InstalledPackage {
    pub id: String,
    pub name: String,
    pub version: String,
    pub package_type: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub installed_at: u64,
    pub has_icon: bool,
    pub has_wasm: bool,
}

/// 内部存储的完整包信息。
struct StoredPackage {
    info: InstalledPackage,
    #[allow(dead_code)]
    raw: AbpPackage,
}

/// 包管理器。
#[derive(Clone)]
pub struct PackageManager {
    inner: Arc<Mutex<PackageManagerInner>>,
}

struct PackageManagerInner {
    packages: HashMap<String, StoredPackage>,
}

impl PackageManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PackageManagerInner {
                packages: HashMap::new(),
            })),
        }
    }

    /// 安装一个包。如果已存在相同 ID，覆盖安装。
    pub fn install(&self, pkg: AbpPackage) -> Result<InstalledPackage, String> {
        let now = current_timestamp_secs();
        let info = InstalledPackage {
            id: pkg.id.clone(),
            name: pkg.manifest.name.clone(),
            version: pkg.manifest.version.clone(),
            package_type: pkg.package_type.as_str().to_string(),
            description: pkg.manifest.description.clone(),
            author: pkg.manifest.author.clone(),
            installed_at: now,
            has_icon: pkg.icon_bytes.is_some(),
            has_wasm: pkg.wasm_bytes.is_some(),
        };

        let stored = StoredPackage {
            info: info.clone(),
            raw: pkg,
        };

        let mut inner = self.inner.lock().map_err(|e| format!("lock: {e}"))?;
        inner.packages.insert(info.id.clone(), stored);
        log::info!("Installed package: {} (type={})", info.id, info.package_type);
        Ok(info)
    }

    /// 卸载一个包。
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|e| format!("lock: {e}"))?;
        if inner.packages.remove(id).is_some() {
            log::info!("Uninstalled package: {}", id);
            Ok(())
        } else {
            Err(format!("package {} not found", id))
        }
    }

    /// 列出所有已安装的包。
    pub fn list(&self) -> Vec<InstalledPackage> {
        let inner = self.inner.lock().unwrap();
        inner.packages.values().map(|p| p.info.clone()).collect()
    }

    /// 按类型列出。
    pub fn list_by_type(&self, pkg_type: PackageType) -> Vec<InstalledPackage> {
        let type_str = pkg_type.as_str();
        let inner = self.inner.lock().unwrap();
        inner
            .packages
            .values()
            .filter(|p| p.info.package_type == type_str)
            .map(|p| p.info.clone())
            .collect()
    }

    /// 获取单个包信息。
    pub fn get(&self, id: &str) -> Option<InstalledPackage> {
        let inner = self.inner.lock().unwrap();
        inner.packages.get(id).map(|p| p.info.clone())
    }

    /// 统计数量。
    pub fn count(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.packages.len()
    }

    /// 按类型统计。
    pub fn count_by_type(&self, pkg_type: PackageType) -> usize {
        self.list_by_type(pkg_type).len()
    }
}

impl Default for PackageManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 获取当前时间戳（秒）。
fn current_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
