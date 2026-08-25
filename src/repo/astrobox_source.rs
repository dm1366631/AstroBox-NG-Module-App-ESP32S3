//! # AstroBox 官方源抓取
//!
//! Primary source：GitHub 仓库 `AstralSightStudios/AstroBox-Repo`
//! - 主索引：`index.csv`（raw）
//! - 备份 CDN：jsdelivr
//! - 每条记录：`name,icon,cover,restype,tags,devices,path,paid_type`
//!
//! 过滤：付费（paid/force_paid）行丢弃；非目标设备行丢弃。
//!
//! 对应验收：**AC7**（paid 过滤）、**AC8**（device 过滤）、**AC9**（manifest 解析拿 URL）

use super::{PaidStatus, RepoItem, RepoManifest, RepoSource, RepoType};
use crate::net_http;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

const INDEX_CSV_PRIMARY: &str =
    "https://raw.githubusercontent.com/AstralSightStudios/AstroBox-Repo/main/index.csv";
const INDEX_CSV_JSDELIVR: &str =
    "https://cdn.jsdelivr.net/gh/AstralSightStudios/AstroBox-Repo@main/index.csv";
const MANIFEST_BASE: &str =
    "https://raw.githubusercontent.com/AstralSightStudios/AstroBox-Repo/main/";

#[derive(Debug, Deserialize)]
struct IndexRow {
    name: String,
    icon: String,
    cover: String,
    restype: String,
    tags: String,
    devices: String,
    path: String,
    #[serde(default, alias = "paid_type")]
    paid_type: String,
}

/// 抓取 AstroBox 官方索引并做基础过滤（paid + device_code）。
///
/// `device_code` 如 `Some("n67")` 用于只显示目标型号支持的条目。
/// `None` 时不做型号过滤（适合设备未连接时浏览全部免费资源）。
pub async fn fetch_index(device_code: Option<&str>) -> Result<Vec<RepoItem>> {
    let csv_text = match net_http::get_text(INDEX_CSV_PRIMARY).await {
        Ok(t) => t,
        Err(_) => net_http::get_text(INDEX_CSV_JSDELIVR)
            .await
            .context("fetch AstroBox index.csv (primary + jsdelivr) both failed")?,
    };

    parse_index_csv(&csv_text, device_code)
}

fn parse_index_csv(text: &str, device_code: Option<&str>) -> Result<Vec<RepoItem>> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());

    let mut out = Vec::with_capacity(rdr.records().size_hint().0.min(500));

    for (idx, row) in rdr.deserialize::<IndexRow>().enumerate() {
        let row = match row {
            Ok(r) => r,
            Err(e) => {
                log::warn!("skip index.csv row {idx}: parse error {e:?}");
                continue;
            }
        };
        let Some(restype) = RepoType::from_csv(&row.restype) else {
            log::debug!("skip row {idx}: unknown restype {}", row.restype);
            continue;
        };
        let paid = PaidStatus::from_csv(&row.paid_type);
        let tags = row
            .tags
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let devices = row
            .devices
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let item = RepoItem {
            name: row.name.trim().to_string(),
            icon_url: row.icon.trim().to_string(),
            cover_url: row.cover.trim().to_string(),
            restype,
            tags,
            devices,
            manifest_path: row.path.trim().to_string(),
            paid,
            source: RepoSource::AstroBoxOfficial,
        };
        if !item.passes_device_and_paid_filter(device_code) {
            continue;
        }
        if item.manifest_path.is_empty() {
            continue;
        }
        out.push(item);
    }
    Ok(out)
}

/// 抓取某个 item 的清单 manifest 并解析成 [`RepoManifest`]。
///
/// AstroBox‑Repo 里 `{path}.json` 的结构常见有两种：
/// 1. 扁平型：直接包含 `repo_url`、`rpk_url` / `watchface_url` 等字段；
/// 2. 嵌套 `payloads` 型：`payloads.quickapp_rpk.url`、`payloads.watchface_mwz.url` 等。
///
/// 我们用一个宽松的 [`RawManifest`] 枚举 + 字段 fallback 提取。
pub async fn fetch_manifest(item: &RepoItem) -> Result<RepoManifest> {
    if item.manifest_path.is_empty() {
        return Err(anyhow!("item has empty manifest_path"));
    }
    // 构造绝对 URL
    let base = url::Url::parse(MANIFEST_BASE).context("parse MANIFEST_BASE")?;
    let full = base
        .join(&item.manifest_path)
        .with_context(|| format!("join manifest path {}", item.manifest_path))?;

    let text = net_http::get_text(full.as_str())
        .await
        .with_context(|| format!("fetch manifest {}", full))?;

    parse_manifest(&text)
}

fn parse_manifest(text: &str) -> Result<RepoManifest> {
    #[derive(Debug, Deserialize)]
    #[derive(Clone)]
    struct PayloadBlock {
        url: Option<String>,
        #[serde(default)]
        filesize: Option<u64>,
    }
    #[derive(Debug, Deserialize)]
    struct Payloads {
        #[serde(default, alias = "quickapp_rpk")]
        quickapp_rpk: Option<PayloadBlock>,
        #[serde(default, alias = "watchface_mwz")]
        watchface_mwz: Option<PayloadBlock>,
        #[serde(default, alias = "watchface")]
        watchface: Option<PayloadBlock>,
    }
    #[derive(Debug, Deserialize)]
    struct RawManifest {
        #[serde(default)]
        repo_url: Option<String>,
        #[serde(default, alias = "rpk_url")]
        rpk_url: Option<String>,
        #[serde(default, alias = "watchface_url")]
        watchface_url: Option<String>,
        #[serde(default)]
        version: Option<String>,
        #[serde(default, alias = "filesize")]
        filesize: Option<u64>,
        #[serde(default, alias = "package_name", alias = "package")]
        package_name: Option<String>,
        #[serde(default, alias = "watchface_id", alias = "id")]
        watchface_id: Option<String>,
        #[serde(default)]
        payloads: Option<Payloads>,
    }

    let raw: RawManifest = serde_json::from_str(text).context("parse manifest JSON")?;

    let (quickapp_rpk_url, filesize_from_payload) =
        if let Some(rpk) = raw.payloads.as_ref().and_then(|p| p.quickapp_rpk.clone()) {
            (rpk.url, rpk.filesize)
        } else {
            (raw.rpk_url.clone(), None)
        };
    let (watchface_url, wf_size) = if let Some(wf) = raw
        .payloads
        .as_ref()
        .and_then(|p| p.watchface_mwz.clone().or_else(|| p.watchface.clone()))
    {
        (wf.url, wf.filesize)
    } else {
        (raw.watchface_url.clone(), None)
    };
    let filesize = raw.filesize.or(filesize_from_payload).or(wf_size);

    Ok(RepoManifest {
        repo_url: raw.repo_url,
        quickapp_rpk_url,
        watchface_url,
        version: raw.version,
        filesize,
        package_name: raw.package_name,
        watchface_id: raw.watchface_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_csv() -> &'static str {
        concat!(
            "name,icon,cover,restype,tags,devices,path,paid_type\n",
            "免费App,https://a.ico,https://a.cover,quickapp,免费;n67,n67;o66,a/app.json,\n",
            "付费App,b.ico,b.cover,quickapp,付费,n67,b/app.json,paid\n",
            "强制App,c.ico,c.cover,watchface,付费,n67,c/app.json,force_paid\n",
            "仅o66,d.ico,d.cover,watchface,,o66,d/app.json,\n",
        )
    }

    #[test]
    fn index_paid_and_device_filters() {
        let all = parse_index_csv(sample_csv(), None).unwrap();
        // 付费 2 条（paid + force_paid）被过滤；剩余 free 2 条
        assert_eq!(all.len(), 2);
        let only_n67 = parse_index_csv(sample_csv(), Some("n67")).unwrap();
        // 仅 o66 的也被过滤；剩下 1 条 免费App
        assert_eq!(only_n67.len(), 1);
        assert_eq!(only_n67[0].name, "免费App");
    }

    #[test]
    fn manifest_flat_parse() {
        let json = r#"{"repo_url":"https://a","rpk_url":"https://a/x.rpk","version":"1.0","filesize":3000,"package_name":"com.foo"}"#;
        let m = parse_manifest(json).unwrap();
        assert_eq!(m.quickapp_rpk_url.as_deref(), Some("https://a/x.rpk"));
        assert_eq!(m.filesize, Some(3000));
        assert_eq!(m.package_name.as_deref(), Some("com.foo"));
    }

    #[test]
    fn manifest_nested_payloads_parse() {
        let json = r#"{"repo_url":"https://w","version":"2","payloads":{"watchface_mwz":{"url":"https://w/x.mwz","filesize":512}}}"#;
        let m = parse_manifest(json).unwrap();
        assert_eq!(m.watchface_url.as_deref(), Some("https://w/x.mwz"));
        assert_eq!(m.filesize, Some(512));
    }
}
