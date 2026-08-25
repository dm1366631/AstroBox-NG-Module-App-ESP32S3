//! 快应用/表盘包解析。
//!
//! 支持两种真实设备格式：
//! - `.rpk`：快应用包，标准 ZIP 容器，内含 `manifest.json`（字段 `package` / `packageName`）
//! - `.bin`：表盘/资源二进制，无内部结构，直接用文件名作为包名
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};
use zip::ZipArchive;

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

/// 解包后的包信息（仅元数据，不保留原始字节）。
#[derive(Clone, Debug)]
pub struct PackageData {
    pub name: String,
    pub version: String,
    pub package_type: PackageType,
    /// 唯一 ID：name + version。
    pub id: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub raw_len: usize,
}

/// manifest.json 的宽松结构（rpk 常见字段）。
#[derive(Deserialize)]
struct RpkManifest {
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    packageName: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

/// 解析上传的包。根据文件名扩展名 / ZIP 魔数自动判别 rpk 或 bin。
pub fn parse_package(file_name: &str, bytes: &[u8]) -> Result<PackageData, String> {
    let lower = file_name.to_lowercase();
    if lower.ends_with(".rpk") || looks_like_zip(bytes) {
        parse_rpk(bytes, file_name)
    } else {
        // 其它（.bin 等）一律按表盘/资源二进制处理
        Ok(parse_bin(file_name, bytes))
    }
}

/// 解析 .rpk（ZIP 容器 + manifest.json）。
fn parse_rpk(bytes: &[u8], file_name: &str) -> Result<PackageData, String> {
    let reader = Cursor::new(bytes);
    let mut archive = ZipArchive::new(reader).map_err(|e| format!("rpk zip open failed: {e}"))?;
    let manifest_index = (0..archive.len())
        .find(|i| {
            archive
                .by_index(*i)
                .ok()
                .map(|f| f.name().ends_with("manifest.json"))
                .unwrap_or(false)
        })
        .ok_or_else(|| "manifest.json not found in rpk".to_string())?;
    let mut manifest_file = archive
        .by_index(manifest_index)
        .map_err(|e| format!("read manifest: {e}"))?;
    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| format!("read manifest content: {e}"))?;
    drop(manifest_file);
    let manifest: RpkManifest =
        serde_json::from_str(&manifest_str).map_err(|e| format!("parse manifest.json: {e}"))?;

    // 包名：优先 manifest.package / packageName / name，兜底文件名
    let raw_name = manifest
        .package
        .or(manifest.packageName)
        .or(manifest.name)
        .unwrap_or_else(|| stem(file_name));
    let name = if raw_name.trim().is_empty() {
        stem(file_name)
    } else {
        raw_name
    };
    let version = manifest
        .version
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "1.0.0".to_string());
    Ok(PackageData {
        id: format!("{}@{}", name, version),
        name,
        version,
        package_type: PackageType::QuickApp,
        description: manifest.description,
        author: manifest.author,
        raw_len: bytes.len(),
    })
}

/// 解析 .bin（表盘/资源二进制，无内部结构）。
fn parse_bin(file_name: &str, bytes: &[u8]) -> PackageData {
    let name = stem(file_name);
    PackageData {
        id: format!("{}@1.0.0", name),
        name,
        version: "1.0.0".to_string(),
        package_type: PackageType::Watchface,
        description: Some("bin 资源/表盘".to_string()),
        author: None,
        raw_len: bytes.len(),
    }
}

fn stem(file_name: &str) -> String {
    std::path::Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name)
        .to_string()
}

/// ZIP 魔数检测：PK\x03\x04 (local header) 或 PK\x05\x06 (empty archive)。
fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && (bytes[..4] == *b"PK\x03\x04" || bytes[..4] == *b"PK\x05\x06")
}
