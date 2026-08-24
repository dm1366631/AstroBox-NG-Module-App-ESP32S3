//! ESP32 内嵌 HTTP 服务器 — 快应用/表盘管理 Web 控制台。
//!
//! 提供 REST API + 简单的 HTML 管理页面。
//! 安装接口用原始字节上传（application/octet-stream），避免 multipart 解析复杂度。

use crate::abp_package::{AbpPackage, PackageType};
use crate::package_manager::{InstalledPackage, PackageManager};
use embedded_svc::http::server::ResponseWrite;
use embedded_svc::io::Read as _;
use esp_idf_svc::http::server::{Configuration, EspHttpServer, Method};
use serde::Serialize;

/// 管理页面 HTML。
const ADMIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>AstroBox - 快应用/表盘管理</title>
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #1a1a2e; color: #eee; padding: 20px; }
  h1 { color: #4fc3f7; margin-bottom: 20px; font-size: 24px; }
  .card { background: #16213e; border-radius: 12px; padding: 20px; margin-bottom: 20px; }
  .card h2 { color: #4fc3f7; margin-bottom: 15px; font-size: 18px; }
  .upload-area { border: 2px dashed #4fc3f7; border-radius: 8px; padding: 30px; text-align: center; cursor: pointer; transition: background 0.2s; }
  .upload-area:hover, .upload-area.dragover { background: rgba(79,195,247,0.1); }
  .upload-area input { display: none; }
  .upload-area p { color: #aaa; margin-top: 10px; }
  .btn { background: #4fc3f7; color: #1a1a2e; border: none; padding: 10px 20px; border-radius: 6px; cursor: pointer; font-weight: bold; margin-top: 10px; }
  .btn:hover { background: #29b6f6; }
  .btn-danger { background: #ef5350; color: #fff; }
  .btn-danger:hover { background: #e53935; }
  .package-list { list-style: none; }
  .package-item { background: #0f3460; border-radius: 8px; padding: 15px; margin-bottom: 10px; display: flex; justify-content: space-between; align-items: center; }
  .package-info h3 { color: #4fc3f7; font-size: 16px; margin-bottom: 5px; }
  .package-info p { color: #aaa; font-size: 13px; }
  .badge { display: inline-block; padding: 2px 8px; border-radius: 4px; font-size: 11px; font-weight: bold; margin-left: 8px; }
  .badge-quick_app { background: #66bb6a; color: #1a1a2e; }
  .badge-watchface { background: #ffa726; color: #1a1a2e; }
  .status-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 15px; }
  .status-item { text-align: center; }
  .status-item .num { font-size: 32px; font-weight: bold; color: #4fc3f7; }
  .status-item .label { color: #aaa; font-size: 13px; margin-top: 5px; }
  #message { padding: 10px 15px; border-radius: 6px; margin-bottom: 15px; display: none; }
  #message.success { background: rgba(102,187,106,0.2); color: #66bb6a; display: block; }
  #message.error { background: rgba(239,83,80,0.2); color: #ef5350; display: block; }
</style>
</head>
<body>
<h1>AstroBox - 快应用/表盘管理</h1>
<div id="message"></div>

<div class="card">
  <h2>系统状态</h2>
  <div class="status-grid" id="status">
    <div class="status-item"><div class="num" id="total-count">-</div><div class="label">已安装总数</div></div>
    <div class="status-item"><div class="num" id="quickapp-count">-</div><div class="label">快应用</div></div>
    <div class="status-item"><div class="num" id="watchface-count">-</div><div class="label">表盘</div></div>
  </div>
</div>

<div class="card">
  <h2>安装 .abp 包</h2>
  <div class="upload-area" id="upload-area">
    <strong>点击选择或拖拽 .abp 文件到此处</strong>
    <p>支持快应用和表盘包（ZIP 格式，含 manifest.json）</p>
    <input type="file" id="file-input" accept=".abp,.zip">
  </div>
  <button class="btn" id="install-btn" disabled>安装</button>
</div>

<div class="card">
  <h2>已安装列表</h2>
  <ul class="package-list" id="package-list">
    <li style="color:#aaa;text-align:center;padding:20px;">加载中...</li>
  </ul>
</div>

<script>
let selectedFile = null;
const uploadArea = document.getElementById('upload-area');
const fileInput = document.getElementById('file-input');
const installBtn = document.getElementById('install-btn');
const messageEl = document.getElementById('message');

function showMessage(text, type) {
  messageEl.textContent = text;
  messageEl.className = type;
  setTimeout(() => { messageEl.className = ''; }, 5000);
}

uploadArea.addEventListener('click', () => fileInput.click());
uploadArea.addEventListener('dragover', (e) => { e.preventDefault(); uploadArea.classList.add('dragover'); });
uploadArea.addEventListener('dragleave', () => uploadArea.classList.remove('dragover'));
uploadArea.addEventListener('drop', (e) => {
  e.preventDefault();
  uploadArea.classList.remove('dragover');
  if (e.dataTransfer.files.length > 0) {
    selectedFile = e.dataTransfer.files[0];
    uploadArea.querySelector('strong').textContent = '已选择: ' + selectedFile.name;
    installBtn.disabled = false;
  }
});
fileInput.addEventListener('change', () => {
  if (fileInput.files.length > 0) {
    selectedFile = fileInput.files[0];
    uploadArea.querySelector('strong').textContent = '已选择: ' + selectedFile.name;
    installBtn.disabled = false;
  }
});

installBtn.addEventListener('click', async () => {
  if (!selectedFile) return;
  installBtn.disabled = true;
  installBtn.textContent = '安装中...';
  try {
    const bytes = await selectedFile.arrayBuffer();
    const resp = await fetch('/api/install', { method: 'POST', body: bytes, headers: { 'Content-Type': 'application/octet-stream' } });
    const data = await resp.json();
    if (resp.ok) {
      showMessage('安装成功: ' + data.name + ' v' + data.version, 'success');
      selectedFile = null;
      uploadArea.querySelector('strong').textContent = '点击选择或拖拽 .abp 文件到此处';
      loadPackages();
    } else {
      showMessage('安装失败: ' + (data.error || resp.statusText), 'error');
    }
  } catch (e) {
    showMessage('网络错误: ' + e, 'error');
  }
  installBtn.textContent = '安装';
  installBtn.disabled = !selectedFile;
});

async function loadPackages() {
  try {
    const [statusResp, pkgResp] = await Promise.all([fetch('/api/status'), fetch('/api/packages')]);
    const status = await statusResp.json();
    const packages = await pkgResp.json();
    document.getElementById('total-count').textContent = status.total;
    document.getElementById('quickapp-count').textContent = status.quick_apps;
    document.getElementById('watchface-count').textContent = status.watchfaces;
    const list = document.getElementById('package-list');
    if (packages.items.length === 0) {
      list.innerHTML = '<li style="color:#aaa;text-align:center;padding:20px;">暂无已安装的包</li>';
      return;
    }
    list.innerHTML = packages.items.map(p => `
      <li class="package-item">
        <div class="package-info">
          <h3>${p.name}<span class="badge badge-${p.package_type}">${p.package_type === 'quick_app' ? '快应用' : '表盘'}</span></h3>
          <p>v${p.version} ${p.author ? '· ' + p.author : ''} ${p.description ? '· ' + p.description : ''}</p>
        </div>
        <button class="btn btn-danger" onclick="uninstall('${p.id}')">卸载</button>
      </li>
    `).join('');
  } catch (e) {
    document.getElementById('package-list').innerHTML = '<li style="color:#ef5350;text-align:center;padding:20px;">加载失败: ' + e + '</li>';
  }
}

async function uninstall(id) {
  if (!confirm('确定要卸载 ' + id + ' 吗？')) return;
  try {
    const resp = await fetch('/api/uninstall', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id })
    });
    const data = await resp.json();
    if (resp.ok) {
      showMessage('卸载成功', 'success');
      loadPackages();
    } else {
      showMessage('卸载失败: ' + (data.error || resp.statusText), 'error');
    }
  } catch (e) {
    showMessage('网络错误: ' + e, 'error');
  }
}

loadPackages();
</script>
</body>
</html>"#;

/// API 响应：状态。
#[derive(Serialize)]
struct StatusResponse {
    total: usize,
    quick_apps: usize,
    watchfaces: usize,
    fw_version: &'static str,
}

/// API 响应：包列表。
#[derive(Serialize)]
struct PackageListResponse {
    items: Vec<InstalledPackage>,
}

/// API 响应：错误。
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// 启动 Web 服务器。
pub fn start_server(pkg_manager: PackageManager) -> Result<EspHttpServer<'static>, Box<dyn std::error::Error>> {
    let config = Configuration {
        http_port: 80,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&config)?;

    let pm = pkg_manager.clone();
    // GET / — 管理页面
    server.fn_handler("/", Method::Get, move |req| {
        let mut resp = req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8")])?;
        resp.write_all(ADMIN_HTML.as_bytes())?;
        Ok(())
    })?;

    let pm_status = pkg_manager.clone();
    // GET /api/status
    server.fn_handler("/api/status", Method::Get, move |req| {
        let status = StatusResponse {
            total: pm_status.count(),
            quick_apps: pm_status.count_by_type(PackageType::QuickApp),
            watchfaces: pm_status.count_by_type(PackageType::Watchface),
            fw_version: env!("CARGO_PKG_VERSION"),
        };
        send_json(req, 200, &status)
    })?;

    let pm_list = pkg_manager.clone();
    // GET /api/packages
    server.fn_handler("/api/packages", Method::Get, move |req| {
        let items = pm_list.list();
        let resp = PackageListResponse { items };
        send_json(req, 200, &resp)
    })?;

    let pm_install = pkg_manager.clone();
    // POST /api/install — body 是 .abp 原始字节
    server.fn_handler("/api/install", Method::Post, move |mut req| {
        // 读取请求体
        let content_len = req
            .header("Content-Length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        if content_len == 0 || content_len > 16 * 1024 * 1024 {
            return send_json(
                req,
                400,
                &ErrorResponse {
                    error: format!("invalid content length: {content_len}"),
                },
            );
        }

        let mut buf = vec![0u8; content_len];
        let mut total_read = 0usize;
        while total_read < content_len {
            match req.read(&mut buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(e) => {
                    return send_json(
                        req,
                        400,
                        &ErrorResponse {
                            error: format!("read body: {e}"),
                        },
                    );
                }
            }
        }
        if total_read < content_len {
            return send_json(
                req,
                400,
                &ErrorResponse {
                    error: format!("incomplete body: {total_read}/{content_len}"),
                },
            );
        }

        match AbpPackage::from_bytes(&buf) {
            Ok(pkg) => match pm_install.install(pkg) {
                Ok(info) => send_json(req, 200, &info),
                Err(e) => send_json(req, 500, &ErrorResponse { error: e }),
            },
            Err(e) => send_json(req, 400, &ErrorResponse { error: e }),
        }
    })?;

    let pm_uninstall = pkg_manager.clone();
    // POST /api/uninstall — JSON body: {id}
    server.fn_handler("/api/uninstall", Method::Post, move |mut req| {
        let content_len = req
            .header("Content-Length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        if content_len == 0 || content_len > 1024 {
            return send_json(
                req,
                400,
                &ErrorResponse {
                    error: "invalid request".to_string(),
                },
            );
        }

        let mut buf = vec![0u8; content_len];
        let mut total_read = 0usize;
        while total_read < content_len {
            match req.read(&mut buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(_) => {
                    return send_json(
                        req,
                        400,
                        &ErrorResponse {
                            error: "read body failed".to_string(),
                        },
                    );
                }
            }
        }
        if total_read < content_len {
            return send_json(
                req,
                400,
                &ErrorResponse {
                    error: "incomplete body".to_string(),
                },
            );
        }

        #[derive(serde::Deserialize)]
        struct UninstallReq {
            id: String,
        }

        match serde_json::from_slice::<UninstallReq>(&buf) {
            Ok(body) => match pm_uninstall.uninstall(&body.id) {
                Ok(()) => send_json(req, 200, &serde_json::json!({"ok": true})),
                Err(e) => send_json(req, 404, &ErrorResponse { error: e }),
            },
            Err(e) => send_json(
                req,
                400,
                &ErrorResponse {
                    error: format!("invalid JSON: {e}"),
                },
            ),
        }
    })?;

    log::info!("Web server started on port 80");
    Ok(server)
}

/// 发送 JSON 响应的辅助函数。
fn send_json<T: Serialize>(
    req: esp_idf_svc::http::server::Request<&mut esp_idf_svc::http::server::EspHttpConnection>,
    status: u16,
    data: &T,
) -> Result<(), std::io::Error> {
    let json = serde_json::to_vec(data).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = req
        .into_response(
            status,
            None,
            &[
                ("Content-Type", "application/json"),
                ("Content-Length", &json.len().to_string()),
            ],
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    resp.write_all(&json)?;
    Ok(())
}
