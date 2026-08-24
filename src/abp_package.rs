//! `.abp` 快应用/表盘包解析。
//!
//! `.abp` 是标准 ZIP 格式，内含 `manifest.json` + 可选的 wasm 入口 + icon。
//! 本模块只做解包和 manifest 校验，不执行 wasm。

use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};
use zip::ZipArchive;

/// 包类型：快应用 or 表盘。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageType {
    QuickApp,
    Watchface,
}

impl PackageType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageType::QuickApp => "quick_app",
            PackageType::Watchface => "watchface",
        }
    }
}

/// `manifest.json` 结构。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// wasm 入口文件名。
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    /// 包类型提示字段。如果没有，根据 name 前缀猜测。
    #[serde(default)]
    pub package_type: Option<String>,
    #[serde(default)]
    pub app_type: Option<String>,
}

/// 解包后的包信息。
#[derive(Clone, Debug)]
pub struct AbpPackage {
    pub manifest: Manifest,
    pub package_type: PackageType,
    /// 生成唯一 ID：name + version。
    pub id: String,
    pub icon_bytes: Option<Vec<u8>>,
    pub wasm_bytes: Option<Vec<u8>>,
}

impl AbpPackage {
    /// 从字节流解析一个 `.abp` 包。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let reader = Cursor::new(bytes);
        let mut archive = ZipArchive::new(reader).map_err(|e| format!("zip open failed: {e}"))?;

        // 找 manifest.json
        let manifest_index = (0..archive.len())
            .find(|i| {
                archive
                    .by_index(*i)
                    .ok()
                    .map(|f| f.name().ends_with("manifest.json"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| "manifest.json not found in package".to_string())?;

        let mut manifest_file = archive
            .by_index(manifest_index)
            .map_err(|e| format!("read manifest: {e}"))?;
        let mut manifest_str = String::new();
        manifest_file
            .read_to_string(&mut manifest_str)
            .map_err(|e| format!("read manifest content: {e}"))?;
        drop(manifest_file);

        let manifest: Manifest =
            serde_json::from_str(&manifest_str).map_err(|e| format!("parse manifest.json: {e}"))?;

        // 判断包类型
        let package_type = Self::detect_type(&manifest);

        // 生成 ID
        let id = format!("{}@{}", manifest.name, manifest.version);

        // 尝试读 icon
        let icon_bytes = Self::read_optional_file(&mut archive, &manifest.icon);
        // 尝试读 wasm
        let wasm_bytes = Self::read_optional_file(&mut archive, &manifest.entry);

        Ok(Self {
            manifest,
            package_type,
            id,
            icon_bytes,
            wasm_bytes,
        })
    }

    fn detect_type(manifest: &Manifest) -> PackageType {
        // 优先看显式字段
        if let Some(t) = &manifest.package_type {
            match t.to_lowercase().as_str() {
                "watchface" | "watch_face" | "表盘" => return PackageType::Watchface,
                "quickapp" | "quick_app" | "快应用" | "app" => return PackageType::QuickApp,
                _ => {}
            }
        }
        if let Some(t) = &manifest.app_type {
            match t.to_lowercase().as_str() {
                "watchface" | "watch_face" => return PackageType::Watchface,
                _ => return PackageType::QuickApp,
            }
        }
        // 根据 name 猜测
        let name = manifest.name.to_lowercase();
        if name.contains("watchface")
            || name.contains("watch_face")
            || name.contains("表盘")
            || name.starts_with("wf_")
        {
            PackageType::Watchface
        } else {
            PackageType::QuickApp
        }
    }

    fn read_optional_file(
        archive: &mut ZipArchive<Cursor<&[u8]>>,
        name: &Option<String>,
    ) -> Option<Vec<u8>> {
        let name = name.as_ref()?;
        let idx = (0..archive.len()).find(|i| {
            archive
                .by_index(*i)
                .ok()
                .map(|f| f.name() == name.as_str() || f.name().ends_with(name.as_str()))
                .unwrap_or(false)
        })?;
        let mut file = archive.by_index(idx).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}
