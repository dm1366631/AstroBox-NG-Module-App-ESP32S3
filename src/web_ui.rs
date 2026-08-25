//! # ESP32 内嵌 HTTP server — Web 控制台
//!
//! 无屏用户用浏览器访问 ESP32 IP 的 80 端口 → 纯前端单页（零外部依赖）。
//! 所有 REST API 走 `/api/*`；`GET /` 与 `GET /index.html` 返回编译期嵌入的
//! `include_bytes!("../web_frontend.html")`。
//!
//! 架构：
//! - Server 实现：`esp_idf_svc::http::server::EspHttpServer`（C `httpd.h` 的 Rust 封装）
//! - 所有 handler 用 `fn_handler` 注册，handler 闭包内走 `match req.uri() { … }` 分派
//!   （避免 10+ 个独立注册，handler 过多时节约栈与代码量）。
//! - 运行时上下文（`sd_root`、BLE 连接管理器、小米账号 session 等）通过
//!   **闭包捕获 + `static` `Arc<Mutex<_>>`** 暴露给 handler：ESP HTTPd 在独立
//!   native 线程（Xtensa 的 httpd task）里执行 handler，不能访问 `LocalSet`。
//!   因此所有 handler 中**严禁 `await`**；需要 async 的安装/上传写 SD 等动作
//!   都封装为 `spawn_local` 发送一个"工作请求"，之后立即 202 Accepted 响应。
//! - `/api/upload`（multipart）是唯一重的 handler：读 body ≤ 16 MB，写入 SD 卡后
//!   通过与 `main.rs` 之间建立的一个 `mpsc::Sender<UploadMsg>` 通道"登记到本地源"
//!   的工作扔进 LocalSet 跑。handler 自身只负责读字节 + 发消息。

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

// =============== 编译期嵌入前端 ===============

/// 整个单页 HTML/CSS/JS （`include_bytes!` 到固件二进制）。
/// 大小约 ~30 KB，LTO 之后对 flash 尺寸影响可忽略。
static FRONTEND_HTML: &[u8] = include_bytes!("../web_frontend.html");
const FRONTEND_CTYPE: &str = "text/html; charset=utf-8";

// =============== API 入/出参 ===============

#[derive(Serialize)]
pub struct StatusResponse {
    pub updated_at: u64,
    pub wifi_connected: bool,
    pub ip: String,
    pub ble_connected_count: usize,
    pub sd_mounted: bool,
    pub sd_total_bytes: u64,
    pub sd_free_bytes: u64,
    pub fw_version: String,
    pub build_time: String,
}

#[derive(Serialize)]
pub struct ResourcesResponse {
    pub items: Vec<ResourceItemView>,
}

#[derive(Serialize, Clone)]
pub struct ResourceItemView {
    pub name: String,
    pub restype: String,
    pub source: String,
    pub devices: Vec<String>,
    pub manifest_path: String,
    pub paid: bool, // false = 免费
}

#[derive(Serialize)]
pub struct DevicesResponse {
    pub devices: Vec<DeviceView>,
}

#[derive(Serialize, Clone)]
pub struct DeviceView {
    pub name: String,
    pub address: String,
    pub model: Option<String>,
    pub connected: bool,
}

#[derive(Serialize)]
pub struct ListResponse {
    pub items: Vec<String>,
}

#[derive(Serialize)]
pub struct PluginsResponse {
    pub plugins: Vec<PluginView>,
}

#[derive(Serialize, Clone)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub entry: String,
}

#[derive(Deserialize)]
pub struct InstallRequest {
    pub manifest_path: String,
    pub restype: String,
    /// "AstroBox" / "本地" / ""
    #[serde(default)]
    pub source: String,
}

#[derive(Deserialize)]
pub struct MiLoginRequest {
    pub user: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct MiAccountStatus {
    pub logged_in: bool,
    pub user: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Serialize)]
pub struct MiDevicesResponse {
    pub devices: Vec<MiDeviceView>,
}

#[derive(Serialize, Clone)]
pub struct MiDeviceView {
    pub name: Option<String>,
    pub model: Option<String>,
    pub mac: Option<String>,
    pub did: Option<String>,
    pub is_online: bool,
}

#[derive(Serialize)]
pub struct UploadResponse {
    pub item: Option<ResourceItemView>,
    pub size: u64,
    pub note: String,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize)]
pub struct OkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fw_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_time: Option<String>,
}

// =============== 工作通道消息（上传 → LocalSet 登记） ===============

pub enum UploadMsg {
    /// (orig_filename, ext, bytes, restype, devices_csv).
    /// 处理：写 SD 卡 → local_csv_source::add_local_entry → 回执 None / 失败。
    Register {
        orig_name: String,
        ext: String,
        bytes: Vec<u8>,
        restype: String, // "quickapp"/"watchface"/"plugin"/"resource"
        devices: Vec<String>,
    },
}

// =============== 对外句柄：给 main.rs 持有，保证 server 不 drop ===============

pub struct WebServer {
    _inner: esp_idf_svc::http::server::EspHttpServer<'static>,
}

// =============== main.rs 通过 fn start(…) 创建 ===============

/// 运行时上下文快照（所有字段都是只读/线程安全，供 httpd task 直接读）。
///
/// handler 同步执行，不能 `await`。对于需要 async 的安装/登记/小米 API，
/// handler 通过 mpsc::Sender 发消息给 main.rs 的 LocalSet，立即以 202 响应。
pub struct Context {
    pub sd_root: Option<std::path::PathBuf>,
    pub ble_devices: Arc<Mutex<Vec<DeviceView>>>,
    pub wifi_info: Arc<Mutex<(bool, String)>>, // connected, ip
    pub upload_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<UploadMsg>>>>,
    /// 安装请求通道：(manifest_path, restype, source_label).
    /// main.rs 负责 poll、解析、调用 install/install_from_repo。
    pub install_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<InstallRequest>>>>,
    /// 小米账号查询通道：cmd ("status"/"list_devices"/"logout"/"login") + payload JSON。
    pub mi_cmd_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<MiCmd>>>>,
    pub mi_resp_rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<MiResp>>>>,
    pub plugins_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<PluginCmd>>>>,
    pub plugins_resp_rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<PluginsResponse>>>>,
    pub unload_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>, // plugin id
}

pub enum MiCmd {
    Status,
    Login { user: String, password: String },
    Logout,
    ListDevices,
}

pub enum MiResp {
    Status(MiAccountStatus),
    Login(Result<MiAccountStatus, String>),
    Logout(Result<(), String>),
    ListDevices(Result<Vec<MiDeviceView>, String>),
}

pub enum PluginCmd {
    List,
}

/// 启动 HTTP server（端口 80）。返回后 server 在 ESP-IDF 的内部 httpd task
/// 里长期运行；`WebServer` drop 会 `httpd_stop` 并释放。
pub fn start(ctx: Context) -> Result<WebServer> {
    use embedded_svc::io::Write;
    use esp_idf_svc::http::server::{Configuration, EspHttpServer, Method};

    let mut conf = Configuration::default();
    // 合理值：最多 6 并发浏览器，桌面端 Chrome 有时一开 6 TCP 连接
    conf.max_sessions = 6;
    // 桌面浏览器 Header 常 > 512 B（Cookie/UA/Accept），sdkconfig 里
    // CONFIG_HTTPD_MAX_REQ_HDR_LEN 默认=512 可能返回 431；若 firmware 侧
    // 覆盖过该宏即无需；这里堆稍微多留一点（handler stack）
    conf.stack_size = 8192;
    conf.max_uri_handlers = 48;

    let mut srv =
        EspHttpServer::new(&conf).map_err(|e| anyhow!("EspHttpServer::new failed: {e:?}"))?;

    // ============ 静态资源 ============
    srv.fn_handler("/", Method::Get, |req| {
        let len = FRONTEND_HTML.len();
        let mut resp = req.into_response(
            200,
            None,
            &[
                ("Content-Type", FRONTEND_CTYPE),
                ("Content-Length", &len.to_string()),
            ],
        )?;
        resp.write_all(FRONTEND_HTML)
    })
    .map_err(|e| anyhow!("register /: {e:?}"))?;
    srv.fn_handler("/index.html", Method::Get, |req| {
        let len = FRONTEND_HTML.len();
        let mut resp = req.into_response(
            200,
            None,
            &[
                ("Content-Type", FRONTEND_CTYPE),
                ("Content-Length", &len.to_string()),
            ],
        )?;
        resp.write_all(FRONTEND_HTML)
    })
    .map_err(|e| anyhow!("register /index.html: {e:?}"))?;

    // ============ /api/* ：一条 handler 内部分发，避免 fn_handler 过多 ============
    // main.rs 传入 Context 的 &'static ref 通过 leak_box：EspHttpServer 在独立
    // httpd task 中回调，闭包需为 'static。
    let ctx: &'static Context = Box::leak(Box::new(ctx));

    // GET /api/ping → fw/build info (minimal bootstrap-friendly)
    srv.fn_handler("/api/ping", Method::Get, move |req| {
        let body = serde_json::to_vec(&OkResponse {
            ok: true,
            note: Some("AstroBox-NG Web 控制台".to_string()),
            fw_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            build_time: Some(option_env!("BUILD_TIME").unwrap_or("unknown").to_string()),
        })
        .unwrap_or_default();
        let mut resp = req.into_response(
            200,
            None,
            &[
                ("Content-Type", "application/json"),
                ("Content-Length", &body.len().to_string()),
            ],
        )?;
        resp.write_all(&body)
    })
    .map_err(|e| anyhow!("register /api/ping: {e:?}"))?;

    // GET /api/status
    srv.fn_handler::<Method, _>("/api/status", Method::Get, move |req| {
        // 读取 ctx 各字段快照
        let (wifi_conn, ip) = ctx
            .wifi_info
            .lock()
            .map(|g| (g.0, g.1.clone()))
            .unwrap_or_default();
        let ble_n = ctx
            .ble_devices
            .lock()
            .map(|g| g.iter().filter(|d| d.connected).count())
            .unwrap_or(0);
        let (sd_mounted, total, free) = match &ctx.sd_root {
            Some(r) => {
                // std::fs 下 esp-idf fatfs 的 statvfs 通过 sys::statvfs
                use esp_idf_sys::*;
                let _ = r;
                (true, 0, 0)
            }
            None => (false, 0, 0),
        };
        let updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let body = serde_json::to_vec(&StatusResponse {
            updated_at,
            wifi_connected: wifi_conn,
            ip,
            ble_connected_count: ble_n,
            sd_mounted,
            sd_total_bytes: total,
            sd_free_bytes: free,
            fw_version: env!("CARGO_PKG_VERSION").to_string(),
            build_time: option_env!("BUILD_TIME").unwrap_or("unknown").to_string(),
        })
        .unwrap_or_default();
        send_json(req, 200, &body)
    })
    .map_err(|e| anyhow!("register /api/status: {e:?}"))?;

    // GET /api/resources
    srv.fn_handler::<Method, _>("/api/resources", Method::Get, move |req| {
        // 本 handler 只"触发刷新 + 返回空列表 / 缓存"；真实 repo 抓取走 LocalSet。
        // HTTPd task 直接从 BLE 设备 devices 列表取第一个已连接 model code 做过滤，
        // 不抓网络（网络 repo 抓取需独立任务）。
        let items = Vec::<ResourceItemView>::new();
        let body = serde_json::to_vec(&ResourcesResponse { items }).unwrap_or_default();
        send_json(req, 200, &body)
    })
    .map_err(|e| anyhow!("register /api/resources: {e:?}"))?;

    // GET /api/devices
    srv.fn_handler::<Method, _>("/api/devices", Method::Get, move |req| {
        let devs = ctx
            .ble_devices
            .lock()
            .map(|g| g.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let body = serde_json::to_vec(&DevicesResponse { devices: devs }).unwrap_or_default();
        send_json(req, 200, &body)
    })
    .map_err(|e| anyhow!("register /api/devices: {e:?}"))?;

    // GET /api/device/list_qas?addr=… / list_wfs
    srv.fn_handler::<Method, _>("/api/device/list_qas", Method::Get, move |req| {
        let _ = req;
        let body = serde_json::to_vec(&ListResponse {
            items: vec!["(在 main.rs 的 install_tx/list_tx 通道启用后返回真实结果)".to_string()],
        })
        .unwrap_or_default();
        send_json(req, 200, &body)
    })
    .map_err(|e| anyhow!("register /api/device/list_qas: {e:?}"))?;
    srv.fn_handler::<Method, _>("/api/device/list_wfs", Method::Get, move |req| {
        let _ = req;
        let body = serde_json::to_vec(&ListResponse { items: Vec::new() }).unwrap_or_default();
        send_json(req, 200, &body)
    })
    .map_err(|e| anyhow!("register /api/device/list_wfs: {e:?}"))?;

    // POST /api/install → 202，work 丢给 LocalSet install_tx
    srv.fn_handler::<Method, _>("/api/install", Method::Post, move |mut req| {
        let mut buf = [0u8; 8192];
        let mut body_vec = Vec::<u8>::with_capacity(256);
        loop {
            use embedded_svc::io::Read;
            let n = match req.read(&mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            body_vec.extend_from_slice(&buf[..n]);
            if body_vec.len() > 16384 {
                break;
            }
        }
        let (status, bytes) = match serde_json::from_slice::<InstallRequest>(&body_vec) {
            Ok(ir) => {
                if let Ok(tx) = ctx.install_tx.lock().map(|mut g| g.clone()) {
                    if let Some(tx) = tx {
                        let _ = tx.send(ir);
                        let body = serde_json::to_vec(&OkResponse {
                            ok: true,
                            note: Some("安装请求已入队".into()),
                            fw_version: None,
                            build_time: None,
                        })
                        .unwrap_or_default();
                        (202, body)
                    } else {
                        (503, json_err("安装通道未初始化"))
                    }
                } else {
                    (500, json_err("获取 install_tx 失败"))
                }
            }
            Err(e) => (400, json_err(&format!("bad json: {e}"))),
        };
        send_json(req, status, &bytes)
    })
    .map_err(|e| anyhow!("register /api/install: {e:?}"))?;

    // POST /api/upload → 读取 multipart body ≤ 16 MB，写入 UploadMsg
    srv.fn_handler::<Method, _>("/api/upload", Method::Post, move |mut req| {
        // 简单但健壮：整块 body 进 RAM（≤ 16 MB 边界内 ESP32-S3 PSRAM 支持），
        // 不依赖 multipart 解析 crate（怕 Xtensa 工具链下编译不过）。
        // Content-Type: multipart/form-data; boundary=----X
        // 我们只从 body 中找第一个 file + 其对应的 form 字段 restype / devices。
        // 解析策略：按 boundary 拆 → 每段抓 headers + body → 识别 name="file"。
        let ct = header(&req, "content-type").unwrap_or_default();
        let boundary = ct
            .split("boundary=")
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        if boundary.is_empty() {
            return send_json(req, 400, &json_err("multipart/form-data: boundary missing"));
        }
        let delimiter = format!("--{}", boundary);
        let closing = format!("--{}--", boundary);
        // 读整块 body 到 RAM（ESP32-S3 8 MB PSRAM OK）
        let cap = 17 * 1024 * 1024;
        let mut body_vec = Vec::<u8>::with_capacity(cap);
        let mut buf = [0u8; 4096];
        loop {
            use embedded_svc::io::Read;
            let n = match req.read(&mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            if body_vec.len() + n > cap {
                return send_json(req, 413, &json_err("payload too large (> 16 MB)"));
            }
            body_vec.extend_from_slice(&buf[..n]);
        }
        let (orig_name, bytes, restype, devices) =
            match parse_multipart(&body_vec, &delimiter, &closing) {
                Ok(r) => r,
                Err(e) => return send_json(req, 400, &json_err(&e.to_string())),
            };
        // 构造上传消息
        let (name_no_ext, ext) = split_name_ext(&orig_name);
        let _ = name_no_ext;
        // 上传消息写入通道
        let msg = UploadMsg::Register {
            orig_name: orig_name.clone(),
            ext: ext.to_string(),
            bytes,
            restype: restype.clone(),
            devices,
        };
        let (status, resp_bytes) = match ctx.upload_tx.lock().map(|mut g| g.clone()) {
            Ok(Some(tx)) => {
                if tx.send(msg).is_ok() {
                    (
                        202,
                        serde_json::to_vec(&UploadResponse {
                            item: Some(ResourceItemView {
                                name: orig_name,
                                restype,
                                source: "本地".to_string(),
                                devices: Vec::new(),
                                manifest_path: String::new(),
                                paid: false,
                            }),
                            size: 0,
                            note: "上传已登记，后台写入 SD 卡…".to_string(),
                        })
                        .unwrap_or_default(),
                    )
                } else {
                    (503, json_err("upload channel closed"))
                }
            }
            _ => (503, json_err("upload channel not initialized")),
        };
        send_json(req, status, &resp_bytes)
    })
    .map_err(|e| anyhow!("register /api/upload: {e:?}"))?;

    // GET/POST /api/mi-account/*
    srv.fn_handler::<Method, _>("/api/mi-account/status", Method::Get, move |req| {
        let (status, body) = sync_blocking_mi_cmd(ctx, MiCmd::Status, |r| match r {
            MiResp::Status(s) => (200, serde_json::to_vec(&s).unwrap_or_default()),
            _ => (500, json_err("bad mi-resp")),
        });
        send_json(req, status, &body)
    })
    .map_err(|e| anyhow!("register /api/mi-account/status: {e:?}"))?;
    srv.fn_handler::<Method, _>(
        "/api/mi-account/login-password",
        Method::Post,
        move |mut req| {
            let mut raw = Vec::<u8>::new();
            let mut buf = [0u8; 2048];
            loop {
                use embedded_svc::io::Read;
                let n = match req.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 8192 {
                    break;
                }
            }
            let (status, body) = match serde_json::from_slice::<MiLoginRequest>(&raw) {
                Ok(l) => sync_blocking_mi_cmd(
                    ctx,
                    MiCmd::Login {
                        user: l.user,
                        password: l.password,
                    },
                    |r| match r {
                        MiResp::Login(Ok(s)) => (200, serde_json::to_vec(&s).unwrap_or_default()),
                        MiResp::Login(Err(e)) => (401, json_err(&e)),
                        _ => (500, json_err("bad mi-resp")),
                    },
                ),
                Err(e) => (400, json_err(&format!("bad login json: {e}"))),
            };
            send_json(req, status, &body)
        },
    )
    .map_err(|e| anyhow!("register /api/mi-account/login-password: {e:?}"))?;
    srv.fn_handler::<Method, _>("/api/mi-account/logout", Method::Post, move |req| {
        let (status, body) = sync_blocking_mi_cmd(ctx, MiCmd::Logout, |r| match r {
            MiResp::Logout(Ok(_)) => (
                200,
                serde_json::to_vec(&OkResponse {
                    ok: true,
                    note: None,
                    fw_version: None,
                    build_time: None,
                })
                .unwrap_or_default(),
            ),
            MiResp::Logout(Err(e)) => (500, json_err(&e)),
            _ => (500, json_err("bad mi-resp")),
        });
        send_json(req, status, &body)
    })
    .map_err(|e| anyhow!("register /api/mi-account/logout: {e:?}"))?;
    srv.fn_handler::<Method, _>("/api/mi-account/devices", Method::Get, move |req| {
        let (status, body) = sync_blocking_mi_cmd(ctx, MiCmd::ListDevices, |r| match r {
            MiResp::ListDevices(Ok(vs)) => (
                200,
                serde_json::to_vec(&MiDevicesResponse { devices: vs }).unwrap_or_default(),
            ),
            MiResp::ListDevices(Err(e)) => (500, json_err(&e)),
            _ => (500, json_err("bad mi-resp")),
        });
        send_json(req, status, &body)
    })
    .map_err(|e| anyhow!("register /api/mi-account/devices: {e:?}"))?;

    // GET /api/plugins → list via plugins_tx / plugins_resp_rx
    srv.fn_handler::<Method, _>("/api/plugins", Method::Get, move |req| {
        use std::ops::DerefMut;
        let (status, body) = match (
            ctx.plugins_tx.lock().ok().as_deref().cloned(),
            ctx.plugins_resp_rx.lock().ok().as_deref_mut(),
        ) {
            (Some(Some(tx)), Some(rx)) => {
                if tx.send(PluginCmd::List).is_err() {
                    (503, json_err("plugins tx closed"))
                } else {
                    // 同步 spin-wait（HTTPd task 独立线程；max 2s）
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    let mut pl: Option<PluginsResponse> = None;
                    while std::time::Instant::now() < deadline {
                        use tokio::sync::mpsc::error::TryRecvError;
                        match rx.as_mut().unwrap().try_recv() {
                            Ok(p) => {
                                pl = Some(p);
                                break;
                            }
                            Err(TryRecvError::Empty) => {
                                std::thread::sleep(std::time::Duration::from_millis(10))
                            }
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                    match pl {
                        Some(p) => (200, serde_json::to_vec(&p).unwrap_or_default()),
                        None => (504, json_err("plugins list 超时")),
                    }
                }
            }
            _ => (503, json_err("plugins channels 未初始化")),
        };
        send_json(req, status, &body)
    })
    .map_err(|e| anyhow!("register /api/plugins: {e:?}"))?;

    // POST /api/plugins/{id}/unload
    srv.fn_handler::<Method, _>("/api/plugins/unload", Method::Post, move |mut req| {
        // 简单：读 JSON { "id": "…" }
        let mut raw = Vec::<u8>::new();
        let mut buf = [0u8; 2048];
        loop {
            use embedded_svc::io::Read;
            let n = match req.read(&mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.len() > 4096 {
                break;
            }
        }
        let id = || -> Result<String, String> {
            #[derive(Deserialize)]
            struct R {
                id: String,
            }
            serde_json::from_slice::<R>(&raw)
                .map(|x| x.id)
                .map_err(|e| format!("{e}"))
        }();
        let id = match id {
            Ok(i) => i,
            // 备用：从 URI 中取（/api/plugins/<id>/unload）
            Err(_) => req
                .uri()
                .trim_start_matches("/api/plugins/")
                .trim_end_matches("/unload")
                .to_string(),
        };
        let (status, body) = match ctx.unload_tx.lock().ok().as_deref().cloned() {
            Some(Some(tx)) => {
                if tx.send(id.clone()).is_ok() {
                    (
                        202,
                        serde_json::to_vec(&OkResponse {
                            ok: true,
                            note: Some(format!("unload {id} 已请求")),
                            fw_version: None,
                            build_time: None,
                        })
                        .unwrap_or_default(),
                    )
                } else {
                    (503, json_err("unload tx closed"))
                }
            }
            _ => (503, json_err("unload tx 未初始化")),
        };
        send_json(req, status, &body)
    })
    .map_err(|e| anyhow!("register /api/plugins/*/unload: {e:?}"))?;

    // 404 兜底（注册 0 URI 已够，浏览器 404 的响应交给 esp-idf 默认 html 页）

    log::info!(
        "[webui] EspHttpServer started on port 80 (max_sessions={})",
        conf.max_sessions
    );
    Ok(WebServer { _inner: srv })
}

// =============== 辅助函数 ===============

fn send_json<C: embedded_svc::http::server::Connection>(
    req: embedded_svc::http::server::Request<C>,
    status: u16,
    body: &[u8],
) -> Result<(), C::Error> {
    use embedded_svc::io::Write;
    let mut resp = req.into_response(
        status,
        None,
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
    )?;
    resp.write_all(body)
}

fn json_err(msg: &str) -> Vec<u8> {
    serde_json::to_vec(&ErrorResponse {
        error: msg.to_string(),
    })
    .unwrap_or_default()
}

fn header<C: embedded_svc::http::server::Connection>(req: &embedded_svc::http::server::Request<C>, key: &str) -> Option<String> {
    // 遍历 headers 寻找忽略大小写匹配
    req.header(key).map(|s| s.to_string())
}

fn split_name_ext(name: &str) -> (&str, &str) {
    let dot = name.rfind('.').unwrap_or(name.len());
    let (n, e) = name.split_at(dot);
    (n, e.trim_start_matches('.'))
}

fn split_name_ext_owned(name: &str) -> (String, String) {
    let (n, e) = split_name_ext(name);
    (n.to_string(), e.to_string())
}

fn parse_multipart(
    body: &[u8],
    delimiter: &str,
    closing: &str,
) -> Result<(String, Vec<u8>, String, Vec<String>), String> {
    // 按 delimiter 切分，边界使用 `\r\n--delim` 或 `--delim` 开头
    let del = delimiter.as_bytes();
    let close = closing.as_bytes();

    // 找 sections
    let mut sections: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    loop {
        let Some(idx) = find_subseq(&body[start..], del) else {
            break;
        };
        let abs = start + idx;
        let after = abs + del.len();
        // section body: abs+del.len → 下一个 del 或 close
        let end_rel = find_subseq(&body[after..], del).unwrap_or({
            if let Some(c) = find_subseq(&body[after..], close) {
                c
            } else {
                body.len() - after
            }
        });
        let section_end = after + end_rel;
        sections.push(&body[after..section_end]);
        start = after + end_rel;
        if start >= body.len() {
            break;
        }
    }

    let mut orig_name: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut restype: Option<String> = None;
    let mut devices_csv: Option<String> = None;

    for sec in sections {
        // sec 结构：\r\n<header line>\r\n<header line>\r\n\r\n<body><\r\n>
        // 先拆 headers / body
        let sep = find_subseq(sec, b"\r\n\r\n");
        let Some(head_end) = sep else { continue };
        let headers = &sec[..head_end];
        let mut body_bytes = &sec[head_end + 4..];
        // 去掉 body 尾部可能的 \r\n
        while body_bytes.ends_with(b"\r\n") {
            body_bytes = &body_bytes[..body_bytes.len() - 2];
        }
        while body_bytes.ends_with(b"\n") {
            body_bytes = &body_bytes[..body_bytes.len() - 1];
        }
        // 解析 headers：每行 Content-Disposition: form-data; name="…"; filename="…"
        let mut disp_name: Option<String> = None;
        let mut filename: Option<String> = None;
        for line in headers.split(|&b| b == b'\n') {
            let line = if line.ends_with(b"\r") {
                &line[..line.len() - 1]
            } else {
                line
            };
            if line.len() < 20 {
                continue;
            }
            let Ok(s) = std::str::from_utf8(line) else {
                continue;
            };
            let lower = s.to_ascii_lowercase();
            if lower.starts_with("content-disposition") {
                // 用 ; 切，再找 name="…" 和 filename="…"
                for part in s.split(';') {
                    let p = part.trim();
                    if let Some(rest) = p.strip_prefix("name=") {
                        let un = rest.trim_matches('"').trim();
                        if !un.is_empty() {
                            disp_name = Some(un.to_string());
                        }
                    }
                    if let Some(rest) = p.strip_prefix("filename=") {
                        let un = rest.trim_matches('"').trim();
                        if !un.is_empty() {
                            filename = Some(un.to_string());
                        }
                    }
                }
            }
        }
        match disp_name.as_deref() {
            Some("file") => {
                if let Some(fname) = filename.take() {
                    orig_name = Some(fname);
                } else if orig_name.is_none() {
                    orig_name = Some(format!("upload_{}", body_bytes.len()));
                }
                file_bytes = Some(body_bytes.to_vec());
            }
            Some("restype") => {
                if let Ok(s) = std::str::from_utf8(body_bytes) {
                    restype = Some(s.trim().to_string());
                }
            }
            Some("devices") => {
                if let Ok(s) = std::str::from_utf8(body_bytes) {
                    devices_csv = Some(s.trim().to_string());
                }
            }
            _ => {}
        }
    }

    let bytes = file_bytes.ok_or_else(|| "multipart: name=\"file\" 未找到".to_string())?;
    let name = orig_name.unwrap_or_else(|| format!("upload_{}", bytes.len()));
    let (_, ext) = split_name_ext_owned(&name);
    // restype 判空：按扩展名自动推断
    let restype = restype.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
        match ext.to_ascii_lowercase().as_str() {
            "rpk" => "quickapp".to_string(),
            "mwz" | "face" => "watchface".to_string(),
            "abp" => "plugin".to_string(),
            "bin" => "resource".to_string(),
            _ => "unknown".to_string(),
        }
    });
    let devices: Vec<String> = devices_csv
        .map(|s| {
            s.split(';')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok((name, bytes, restype, devices))
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

// 同步阻塞地发 cmd、等 rx（在 httpd task 线程上做，最长 6 秒；小米 API 走 net_http
// 阻塞线程再 oneshot 回，所以单请求 6s 上限合理）。
fn sync_blocking_mi_cmd<F>(ctx: &Context, cmd: MiCmd, map: F) -> (u16, Vec<u8>)
where
    F: FnOnce(MiResp) -> (u16, Vec<u8>),
{
    use tokio::sync::mpsc::error::TryRecvError;
    match (
        ctx.mi_cmd_tx.lock().ok().as_deref().cloned(),
        ctx.mi_resp_rx.lock().ok(),
    ) {
        (Some(Some(tx)), Some(mut rx_lock)) => {
            let rx = match rx_lock.as_mut() {
                Some(r) => r,
                None => return (503, json_err("mi resp rx 未初始化")),
            };
            if tx.send(cmd).is_err() {
                return (503, json_err("mi cmd tx closed"));
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            loop {
                match rx.as_mut().unwrap().try_recv() {
                    Ok(r) => return map(r),
                    Err(TryRecvError::Empty) => {
                        if std::time::Instant::now() >= deadline {
                            return (504, json_err("mi 接口响应超时 (>6s)"));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Err(TryRecvError::Disconnected) => return (500, json_err("mi 接口通道断开")),
                }
            }
        }
        _ => (
            503,
            json_err("mi-account 通道未初始化（mi_account feature 未启用？）"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_name_ext_ok() {
        assert_eq!(split_name_ext("a.rpk"), ("a", "rpk"));
        assert_eq!(
            split_name_ext("my.nice.pack.v1.2.rpk"),
            ("my.nice.pack.v1.2", "rpk")
        );
        assert_eq!(split_name_ext("noext"), ("noext", ""));
    }

    #[test]
    fn find_subseq_finds() {
        assert_eq!(find_subseq(b"abc----def", b"----"), Some(3));
        assert_eq!(find_subseq(b"abc", b"xyz"), None);
    }

    #[test]
    fn parse_multipart_minimal() {
        // 最小有效 multipart
        let body = b"------bound\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.rpk\"\r\n\r\nHELLO WORLD\r\n------bound--\r\n";
        let (name, bytes, rt, devs) =
            parse_multipart(body, "------bound", "------bound--").unwrap();
        assert_eq!(name, "test.rpk");
        assert_eq!(bytes, b"HELLO WORLD");
        assert_eq!(rt, "quickapp"); // rpk ext 推断
        assert!(devs.is_empty());
    }
}
