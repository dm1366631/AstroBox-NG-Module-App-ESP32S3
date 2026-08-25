#![allow(dead_code)]
//! AstroBox-NG Module firmware entry point.
//!
//! This crate wires together Wi-Fi, BLE, NVS configuration, OTA stub,
//! GUI rendering, MicroSD logging / local install, network repo sources
//! (AstroBox official only; BandBBS removed for ToS compliance) and
//! device-to-device transfer APIs for the ESP32-S3.
//!
//! Public functions in this file serve as a thin host-facing API surface
//! (for future remote-control / RPC integration) and are annotated with
//! `#[allow(dead_code)]` at the crate level because they are not yet
//! invoked by any internal code path.

use core::convert::TryInto;

use anyhow::anyhow;
use corelib::device::xiaomi::{
    components::{info::InfoSystem, network::NetworkComponent},
    XiaomiDevice,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::gpio::{Gpio8, Gpio9, PinDriver, Pins},
    hal::modem::Modem,
    hal::prelude::Peripherals,
    hal::spi::SpiDriver,
    io::vfs::MountedEventfs,
    log::EspLogger,
    nvs::EspDefaultNvsPartition,
    sys::link_patches,
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi},
};
use log::LevelFilter;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod allocator;
pub mod install;
pub mod local_packages;
pub mod logging;
pub mod mi_account;
pub mod miwear;
pub mod net_http;
pub mod nvs_config;
pub mod package_format;
pub mod ota;
#[cfg(feature = "plugin_runtime")]
pub mod plugin_runtime;
pub mod repo;
pub mod sdcard;
pub mod statlogger;
pub mod transfer;
/// 无屏用户网页控制台：`#[cfg(feature = "webui")]` 开关，出货关闭零额外空间。
/// 打开后 ESP32 端口 80 起 HTTP server，编译期嵌入前端单页。
/// 所有 handler 闭包为 'static（ESP-IDF httpd 独立 task），需要的运行时
/// 上下文通过 `Arc<Mutex<_>>` + `mpsc` 通道暴露，见 [`web_ui::Context`]。
#[cfg(feature = "webui")]
pub mod web_ui;

const WIFI_RECONNECT_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const WIFI_INIT_RETRY_DELAY: Duration = Duration::from_secs(5);
const WIFI_INIT_MAX_RETRIES: u32 = 5;
const OTA_CHECK_INTERVAL: Duration = Duration::from_secs(3600);
const ECS_STACK_SIZE: usize = 32 * 1024;

// ===== Web UI 共享静态：Wi-Fi 连接状态 + STA IP =====
// （不走 NVS 接口，直接用 Atomic 由 wifi_reconnect_watchdog 周期刷新）
static WIFI_CONNECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WIFI_STA_IP: std::sync::OnceLock<std::sync::RwLock<String>> = std::sync::OnceLock::new();

#[cfg(feature = "webui")]
fn nvs_config_is_wifi_connected() -> bool {
    WIFI_CONNECTED.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(feature = "webui")]
fn nvs_config_wifi_sta_ip() -> Result<String, String> {
    WIFI_STA_IP
        .get_or_init(|| std::sync::RwLock::new(String::new()))
        .read()
        .map(|g| g.clone())
        .map_err(|e| format!("{e:?}"))
}

fn main() -> anyhow::Result<()> {
    link_patches();
    // 先启动 EspLogger（串口侧）作为 fallback，随后会被
    // install_combined_logger() 替换为"串口 + SD 文件"的组合。
    EspLogger::initialize_default();
    configure_component_log_levels();

    let _mounted_eventfs = MountedEventfs::mount(5)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, run_app())
}

/// 运行期共享状态：`resource_panel_event_loop` 需要访问 SD 挂载信息；
/// 当前用 Rc<RefCell<_>> 包裹（跑在 Tokio single-threaded runtime 内
/// 所以不需要 Send/Sync）。未来如果需要跨线程，改成 Arc<Mutex<_>>。
struct AppSharedState {
    sd: Option<sdcard::SdCard>,
    /// `/sdcard` 作为 `&'static Path` 直接引用（`SDCARD_ROOT` 常量）。
    /// 当 sd 未挂载时为 None。
    sd_root: Option<&'static Path>,
}

async fn run_app() -> anyhow::Result<()> {
    nvs_config::ensure_nvs_initialized();

    // ===== 1. 外设解包 =====
    let Peripherals {
        pins,
        ledc,
        spi2,
        i2c0,
        modem,
        ..
    } = Peripherals::take()?;

    let Pins {
        gpio0,  // TP RST
        gpio1,  // TP INT
        gpio2,  // LCD BL
        gpio3,  // LCD RST
        gpio4,  // LCD DC
        gpio5,  // LCD CS
        gpio6,  // SPI2 MOSI
        gpio7,  // SPI2 SCLK
        gpio8,  // SPI2 MISO  ← SD 新增
        gpio9,  // SD CS      ← SD 新增
        gpio18, // I2C SDA
        gpio16, // I2C SCL
        ..
    } = pins;

    // ===== 2. Wi-Fi（先起来，便于 SD 日志拿 NTP 时间；也为 repo_net 做准备） =====
    //      凭据为空 → 进入 AP 配置模式（开热点 AstroBox-Setup，用户配完自动重启进 STA）。
    let (wifi_ssid, wifi_password) = nvs_config::load_wifi_credentials();
    let setup_mode = wifi_ssid.is_empty();
    let mut wifi = if setup_mode {
        log::warn!("未配置 WiFi 凭据，进入 AP 配置模式：热点 AstroBox-Setup @ 192.168.4.1");
        init_wifi_ap_mode(modem)?
    } else {
        init_wifi_with_retry(modem, &wifi_ssid, &wifi_password).await?
    };

    if setup_mode {
        // AP 配置模式：保留 wifi 句柄（热点保持），不启动 STA 重连 watchdog。
        // 用户在 Web 页面提交凭据后由下方 setup 循环触发重启。
        let _ = &wifi;
    } else {
        if let Err(e) = nvs_config::save_wifi_credentials(&wifi_ssid, &wifi_password) {
            log::debug!("Initial Wi-Fi credentials save skipped: {e}");
        }
        tokio::task::spawn_local(async move {
            wifi_reconnect_watchdog(wifi, wifi_ssid, wifi_password).await;
        });
    }

    // ===== 3. SNTP：让日志 / 文件修改时间接近真实 UTC =====
    // sdkconfig.defaults 已经开启 CONFIG_LWIP_SNTP_ENABLED=y；这里做一次
    // best-effort 初始化，失败忽略（fallback 到 epoch 秒，不影响主流程）。
    spawn_sntp_init_best_effort();

    // ===== 4. SPI2 共享总线驱动（SCLK=GPIO7, MOSI=GPIO6, MISO=GPIO8） =====
    //      LCD (CS=GPIO5) 和 SD 卡 (CS=GPIO9) 分别创建独立 SpiDeviceDriver。
    let shared_spi: SpiDriver<'static> = sdcard::new_spi2_bus_driver(spi2, gpio7, gpio6, gpio8)?;

    // ===== 5. 尝试挂载 SD 卡（CS=GPIO9）；失败降级（sd=None，只打串口日志） =====
    // ble-web 构建已禁用 SD 卡（esp-idf-svc 0.51 移除 sdmmc API），直接降级为 None。
    let (maybe_sd, sd_root): (Option<sdcard::SdCard>, Option<&'static Path>) = (None, None);
    let _ = &shared_spi;
    let _ = gpio8;
    let _ = gpio9;

    // ===== 6. 安装日志后端（串口 + SD 滚动文件；SD 挂失败时仅串口） =====
    if let Err(e) = logging::install_combined_logger(sd_root, LevelFilter::Debug) {
        // 多半是 EspLogger 已经 set_boxed_logger。保持运行即可。
        log::warn!("combined logger install failed (existing logger?): {e:#}");
    }

    // ===== 7. OTA =====
    let ota_manager = std::sync::Arc::new(ota::OtaManager::new());
    {
        let mgr = ota_manager.clone();
        tokio::task::spawn_local(async move {
            ota_check_loop(mgr).await;
        });
    }

    if let Some(initial_ota) = ota_manager.check_for_update() {
        log::debug!(
            "OTA update available: v{} ({} bytes)",
            initial_ota.version,
            initial_ota.size
        );
    }

    // ===== 8. ECS + UI 初始化 =====
    corelib::ecs::init_runtime_default_with_stack(ECS_STACK_SIZE);

    // ===== 9. 周期性：堆统计 / 网络计费 =====
    tokio::task::spawn_local(async {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            ticker.tick().await;
            statlogger::log_heap_info();
            log_network_meter().await;
        }
    });

    // ===== 13. MiWear BLE =====
    tokio::task::spawn_local(async {
        if let Err(err) = miwear::connect_with_retry().await {
            log::error!("miwear connect loop exited: {err:?}");
        }
    });

    // ===== 14. 周期：ECS 安装列表同步 + 设备 roster =====
    tokio::task::spawn_local(async {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            sync_installed_items().await;
            log_device_roster().await;
        }
    });

    // ===== 14.0 shared_state：先初始化，后续资源 UI + Web UI 通道共同使用 =====
    //      maybe_sd / sd_root 已绑定，先 move 到 Rc<RefCell<_>> 共享。
    let shared_state = std::rc::Rc::new(std::cell::RefCell::new(AppSharedState {
        sd: maybe_sd,
        sd_root,
    }));
    // sd_root Option<&'static Path> 也保留一份给 web UI 初始化（不依赖 Rc）
    let sd_root_opt: Option<&'static Path> = { shared_state.borrow().sd_root };
    // ===== 14.1 Web UI 通道 + 共享 Arc 快照（feature=webui 时启用） =====
    #[cfg(feature = "webui")]
    let mut webui_install_rx: Option<tokio::sync::mpsc::UnboundedReceiver<web_ui::InstallRequest>>;
    #[cfg(feature = "webui")]
    let mut webui_upload_rx: Option<tokio::sync::mpsc::UnboundedReceiver<web_ui::UploadMsg>>;
    #[cfg(feature = "webui")]
    let mut webui_mi_rx: Option<tokio::sync::mpsc::UnboundedReceiver<web_ui::MiCmd>>;
    #[cfg(feature = "webui")]
    let mut webui_plugins_rx: Option<tokio::sync::mpsc::UnboundedReceiver<web_ui::PluginCmd>>;
    #[cfg(feature = "webui")]
    let mut webui_unload_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>;
    #[cfg(feature = "webui")]
    {
        use std::sync::{Arc, Mutex};
        // 1) 通道
        let (install_tx, install_rx_ch) =
            tokio::sync::mpsc::unbounded_channel::<web_ui::InstallRequest>();
        let (upload_tx, upload_rx_ch) = tokio::sync::mpsc::unbounded_channel::<web_ui::UploadMsg>();
        let (mi_cmd_tx, mi_rx_ch) = tokio::sync::mpsc::unbounded_channel::<web_ui::MiCmd>();
        let (mi_resp_tx, mi_resp_rx_ch) = tokio::sync::mpsc::unbounded_channel::<web_ui::MiResp>();
        let (plugins_tx, plugins_rx_ch) =
            tokio::sync::mpsc::unbounded_channel::<web_ui::PluginCmd>();
        let (plugins_resp_tx, plugins_resp_rx_ch) =
            tokio::sync::mpsc::unbounded_channel::<web_ui::PluginsResponse>();
        let (unload_tx, unload_rx_ch) = tokio::sync::mpsc::unbounded_channel::<String>();
        webui_install_rx = Some(install_rx_ch);
        webui_upload_rx = Some(upload_rx_ch);
        webui_mi_rx = Some(mi_rx_ch);
        webui_plugins_rx = Some(plugins_rx_ch);
        webui_unload_rx = Some(unload_rx_ch);

        // 2) WiFi/BLE 共享快照（与 web UI 主线程 httpd task 共享）
        let wifi_info = Arc::new(Mutex::new((false, String::new())));
        let ble_devices = Arc::new(Mutex::new(Vec::<web_ui::DeviceView>::new()));
        // 周期刷新快照
        {
            let wifi_info = wifi_info.clone();
            tokio::task::spawn_local(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tick.tick().await;
                    let connected = nvs_config_is_wifi_connected();
                    let ip = nvs_config_wifi_sta_ip().unwrap_or_default();
                    if let Ok(mut g) = wifi_info.lock() {
                        g.0 = connected;
                        g.1 = ip;
                    }
                }
            });
        }
        {
            let ble_devices = ble_devices.clone();
            tokio::task::spawn_local(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(6));
                loop {
                    tick.tick().await;
                    // 设备列表：从 transfer / ecs 拿 addr, name, connected
                    let addrs = transfer::list_connected_devices().await;
                    let known: Vec<web_ui::DeviceView> = addrs
                        .iter()
                        .map(|a| web_ui::DeviceView {
                            name: guess_device_name_from_addr(a)
                                .unwrap_or_else(|| "Mi Band".to_string()),
                            address: a.clone(),
                            model: None,
                            connected: true,
                        })
                        .collect();
                    if let Ok(mut g) = ble_devices.lock() {
                        *g = known;
                    }
                }
            });
        }

        // 3) EspHttpServer start — **在独立 OS 线程**（不是 spawn_local），因为
        //    `EspHttpServer::new(...)` 会立刻启动 httpd task 并同步注册 handler；
        //    只要不 drop WebServer，server 一直在后台。将 `_server` 放进 Box::leak
        //    以保证整个固件生命周期存活。
        let sd_root_pb: Option<std::path::PathBuf> = sd_root_opt.map(|p| p.to_path_buf());
        let ctx = web_ui::Context {
            setup_mode,
            sd_root: sd_root_pb,
            ble_devices: ble_devices.clone(),
            wifi_info,
            upload_tx: Arc::new(Mutex::new(Some(upload_tx))),
            install_tx: Arc::new(Mutex::new(Some(install_tx))),
            mi_cmd_tx: Arc::new(Mutex::new(Some(mi_cmd_tx))),
            mi_resp_rx: Arc::new(Mutex::new(Some(mi_resp_rx_ch))),
            plugins_tx: Arc::new(Mutex::new(Some(plugins_tx))),
            plugins_resp_rx: Arc::new(Mutex::new(Some(plugins_resp_rx_ch))),
            unload_tx: Arc::new(Mutex::new(Some(unload_tx))),
        };
        std::thread::Builder::new()
            .name("webui-server".to_string())
            .stack_size(16 * 1024)
            .spawn(move || match web_ui::start(ctx) {
                Ok(srv) => {
                    log::info!("[webui] server thread OK, leaking server handle");
                    Box::leak(Box::new(srv));
                }
                Err(e) => log::warn!("[webui] start FAILED — disabled. {e:?}"),
            })
            .expect("webui server thread spawn");

        // 5) AP 配置模式：轮询用户是否已通过 Web 页面提交 WiFi 凭据，提交后重启进 STA
        if setup_mode {
            tokio::task::spawn_local(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tick.tick().await;
                    if web_ui::SETUP_REQUESTED.load(std::sync::atomic::Ordering::Acquire) {
                        log::warn!("WiFi 凭据已保存，重启进入 STA 模式...");
                        std::thread::sleep(Duration::from_millis(600));
                        unsafe { esp_idf_sys::esp_restart(); }
                    }
                }
            });
        }

        // 4) 工作任务：轮询各通道并在 LocalSet 上跑真实 async 逻辑
        // 4a) install worker: 调 install_* / install_from_repo 或 local_packages::install_local
        let shared_state_w = shared_state.clone();
        let mut irx = webui_install_rx.take().unwrap();
        tokio::task::spawn_local(async move {
            while let Some(req) = irx.recv().await {
                // 解析 restype + source，决定走哪条 install 路径
                let restype_l = req.restype.to_ascii_lowercase();
                let is_local = req.source.trim() == "本地";
                let shared = shared_state_w.clone();
                tokio::task::spawn_local(async move {
                    let _ = do_webui_install(req, restype_l, is_local, shared).await;
                });
            }
        });

        // 4b) upload worker: 解析上传的 .rpk/.bin 并直接通过 ABNG BLE 安装到设备。
        //     不依赖 SD 卡（ble-web 全 Web 管理、无 SD 卡依赖）。
        let shared_state_u = shared_state.clone();
        let ble_devices_u = ble_devices.clone();
        let mut urx = webui_upload_rx.take().unwrap();
        tokio::task::spawn_local(async move {
            while let Some(web_ui::UploadMsg::Register {
                orig_name,
                ext: _ext,
                bytes,
                restype: _restype,
                devices,
            }) = urx.recv().await
            {
                let ble_devices = ble_devices_u.clone();
                let _ = shared_state_u.clone();
                tokio::task::spawn_local(async move {
                    let pkg = match crate::package_format::parse_package(&orig_name, &bytes) {
                        Ok(p) => p,
                        Err(e) => {
                            log::error!("[webui/upload] 解析 {} 失败: {}", orig_name, e);
                            return;
                        }
                    };
                    // 目标设备：上传时指定，否则所有已连接设备
                    let targets: Vec<String> = if !devices.is_empty() {
                        devices.clone()
                    } else {
                        ble_devices
                            .lock()
                            .map(|g| {
                                g.iter()
                                    .filter(|d| d.connected)
                                    .map(|d| d.address.clone())
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    if targets.is_empty() {
                        log::warn!("[webui/upload] 没有可安装的目标设备（无已连接设备）");
                        return;
                    }
                    for addr in &targets {
                        let result = match pkg.package_type {
                            crate::package_format::PackageType::Watchface => {
                                crate::install::install_watchface(addr, bytes.clone()).await
                            }
                            crate::package_format::PackageType::QuickApp => {
                                crate::install::install_quick_app(addr, &pkg.name, bytes.clone())
                                    .await
                            }
                        };
                        match result {
                            Ok(()) => {
                                log::info!("[webui/upload] 已安装 {} 到 {}", pkg.name, addr)
                            }
                            Err(e) => log::error!(
                                "[webui/upload] 安装 {} 到 {} 失败: {:#}",
                                pkg.name,
                                addr,
                                e
                            ),
                        }
                    }
                });
            }
        });

        // 4c) mi account worker
        let mut mirx = webui_mi_rx.take().unwrap();
        tokio::task::spawn_local(async move {
            while let Some(cmd) = mirx.recv().await {
                let resp = match cmd {
                    web_ui::MiCmd::Status => {
                        let (ok, user, uid) = match mi_account::load_session() {
                            Some(s) => (true, Some(s.user_id.clone()), Some(s.user_id)),
                            None => (false, None, None),
                        };
                        web_ui::MiResp::Status(web_ui::MiAccountStatus {
                            logged_in: ok,
                            user: user.clone(),
                            user_id: uid,
                        })
                    }
                    web_ui::MiCmd::Login { user, password } => web_ui::MiResp::Login(
                        match mi_account::login_with_password(&user, &password).await {
                            Ok(mi_account::LoginResult::Ok(sess)) => Ok(web_ui::MiAccountStatus {
                                logged_in: true,
                                user: Some(sess.user_id.clone()),
                                user_id: Some(sess.user_id),
                            }),
                            Ok(mi_account::LoginResult::NeedSms { desc, .. }) => Err(desc),
                            Err(e) => Err(format!("{e:#}")),
                        },
                    ),
                    web_ui::MiCmd::Logout => {
                        web_ui::MiResp::Logout(mi_account::logout().map_err(|e| format!("{e:#}")))
                    }
                    web_ui::MiCmd::ListDevices => {
                        let result = match mi_account::load_session() {
                            Some(sess) => mi_account::list_devices(&sess).await,
                            None => Err(anyhow::anyhow!("未登录，请先登录小米账号")),
                        };
                        web_ui::MiResp::ListDevices(match result {
                            Ok(list) => Ok(list
                                .into_iter()
                                .map(|d| web_ui::MiDeviceView {
                                    name: Some(d.name),
                                    model: Some(d.model),
                                    mac: Some(d.mac),
                                    did: Some(d.device_id),
                                    is_online: d.is_online,
                                })
                                .collect()),
                            Err(e) => Err(format!("{e:#}")),
                        })
                    }
                };
                let _ = mi_resp_tx.send(resp);
            }
        });

        // 4d) plugins list / unload workers
        let mut prx = webui_plugins_rx.take().unwrap();
        tokio::task::spawn_local(async move {
            while let Some(web_ui::PluginCmd::List) = prx.recv().await {
                #[cfg(feature = "plugin_runtime")]
                {
                    let ps = plugin_runtime::list();
                    let _ = plugins_resp_tx.send(web_ui::PluginsResponse {
                        plugins: ps
                            .into_iter()
                            .map(|p| web_ui::PluginView {
                                id: p.id,
                                name: p.name,
                                version: p.version,
                                entry: p.entry,
                            })
                            .collect(),
                    });
                }
                #[cfg(not(feature = "plugin_runtime"))]
                {
                    let _ = plugins_resp_tx.send(web_ui::PluginsResponse { plugins: vec![] });
                }
            }
        });
        let mut urx_un = webui_unload_rx.take().unwrap();
        tokio::task::spawn_local(async move {
            while let Some(id) = urx_un.recv().await {
                #[cfg(feature = "plugin_runtime")]
                {
                    let _ = plugin_runtime::unload(&id);
                }
                let _ = id;
            }
        });

        // 5) 让 UI 顶部提示 IP（有屏也显示，便于一起抄）
        if let Ok(ip) = nvs_config_wifi_sta_ip() {
            if !ip.is_empty() {
                log::info!("Web 控制台：http://{ip}/");
            }
        }
    }
    #[cfg(not(feature = "webui"))]
    {
        webui_install_rx = None;
        webui_upload_rx = None;
        webui_mi_rx = None;
        webui_plugins_rx = None;
        webui_unload_rx = None;
        let _ = (&wifi_ssid, shared_state); // silence unused
    }

    tokio::task::spawn_local(async {
        let mut ticker = tokio::time::interval(Duration::from_secs(10));
        let mut last_count: usize = 0;
        loop {
            ticker.tick().await;
            let devices =
                corelib::ecs::with_rt_mut(|rt| rt.device_ids().cloned().collect::<Vec<_>>()).await;
            if devices.len() != last_count {
                log::info!(
                    "[Transfer] Device roster: {} device(s) connected → {:?}",
                    devices.len(),
                    devices
                );
                last_count = devices.len();
            }
        }
    });

    Ok(())
}

// =====================================================================
async fn first_connected_device_addr() -> Option<String> {
    let ids = transfer::list_connected_devices().await;
    ids.into_iter().next()
}

/// 尽力而为：取第一台连接设备的 `device_code`（n67 / o66 等）。
/// 失败 / 未连接 返回 None（不做设备型号过滤，会显示所有免费条目）。
async fn first_connected_device_model_code() -> Option<String> {
    let addr = first_connected_device_addr().await?;
    // corelib 目前没有暴露"device_model"字段，我们尝试通过设备名前缀粗略匹配：
    // - 设备名 "Mi Band 9" → code 猜测 "n67"
    // - 设备名 "Redmi Watch 5" → code 猜测 "o66"
    // 但更准确的方式应是从 ECS XiaomiDevice.model 读。这里先拿 name 匹配。
    let name = transfer::get_device_info(&addr).await.ok()?;
    let lower = name.to_ascii_lowercase();
    // 常见映射（可按需增补）
    if lower.contains("band 10") || lower.contains("miband10") || lower.contains("mi band 10") {
        Some("n75".into())
    } else if lower.contains("band 9") {
        Some("n67".into())
    } else if lower.contains("redmi watch 6") {
        Some("o72".into())
    } else if lower.contains("redmi watch 5") {
        Some("o66".into())
    } else if lower.contains("watch s5") {
        Some("s5".into())
    } else if lower.contains("watch s4") {
        Some("s4".into())
    } else if lower.contains("watch s3") {
        Some("s3".into())
    } else {
        None
    }
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.2} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Web UI `do_webui_install`：把 web_ui::InstallRequest 分发到已有
/// `spawn_install_task` / `local_packages::install_local` 链路。
/// 尽量复用已有的安装 pipeline，避免两条路径分叉。
async fn do_webui_install(
    req: web_ui::InstallRequest,
    restype_l: String,
    is_local: bool,
    shared: std::rc::Rc<std::cell::RefCell<AppSharedState>>,
) -> anyhow::Result<()> {
    // 1) 取目标设备地址
    let addr = match first_connected_device_addr().await {
        Some(a) => a,
        None => {
            log::warn!("未连接设备，先配对手环再安装");
            return Err(anyhow!("no connected device"));
        }
    };

    // 2) 本地源安装：直接用 local_packages
    if is_local {
        // 找 SD 上对应 manifest_path（约定是 "<abs_file>.json"，真正的文件是去掉 .json）
        let file_path = req
            .manifest_path
            .strip_suffix(".json")
            .unwrap_or(&req.manifest_path)
            .to_string();
        let ext = std::path::Path::new(&file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        let Some(kind) = local_packages::classify(&ext) else {
            return Err(anyhow!("local install: unknown extension {ext}"));
        };
        let lp = local_packages::LocalPackage {
            name: std::path::Path::new(&file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("local")
                .to_string(),
            path: std::path::PathBuf::from(&file_path),
            size: tokio::fs::metadata(&file_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0),
            modified_at: tokio::fs::metadata(&file_path)
                .await
                .ok()
                .and_then(|m| m.modified().ok()),
            r#type: kind,
            guessed_pkg_name: None,
        };
        return local_packages::install_local(&addr, &lp, None).await;
    }

    // 3) AstroBox 官方源：构造 ListEntry::Repo 然后复用 spawn_install_task 的逻辑
    //    （这里不直接调 spawn_install_task 因为已经在 spawn_local 里）
    let restype = if restype_l.contains("watch") || restype_l == "face" {
        crate::repo::RepoType::Watchface
    } else {
        crate::repo::RepoType::QuickApp
    };
    let item = crate::repo::RepoItem {
        name: req.manifest_path.clone(),
        icon_url: String::new(),
        cover_url: String::new(),
        restype,
        tags: vec![],
        devices: vec![],
        manifest_path: req.manifest_path,
        paid: crate::repo::PaidStatus::Free,
        source: crate::repo::RepoSource::AstroBoxOfficial,
    };
    #[cfg(feature = "repo_net")]
    {
        let sd_root = shared.borrow().sd_root;
        let cache = sd_root.is_some();
        let manifest = crate::repo::astrobox_source::fetch_manifest(&item).await?;
        crate::install::install_from_repo(&addr, &item, &manifest, cache, sd_root, None)
            .await
            .map(|_| ())
    }
    #[cfg(not(feature = "repo_net"))]
    {
        let _ = (&addr, &item, shared);
        Err(anyhow!("repo_net feature disabled"))
    }
}

/// 根据 BLE 地址猜名字（优先从 corelib ecs 查；查不到给一个兜底）。
/// 当前 corelib 只暴露 device_ids addr，没暴露 name，所以先返回
/// "Mi Band + addr 末 2 字节" 这种友好形式，接入真实 roster 后再改。
fn guess_device_name_from_addr(addr: &str) -> Option<String> {
    let last4: String = addr
        .rsplit(':')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    if last4.is_empty() {
        None
    } else {
        Some(format!("Mi Band …{last4}"))
    }
}

/// 用 ESP-IDF FFI `esp_netif_get_ip_info` 读取 STA IP。
/// 失败/未连接 返回 None，不做 panic。
fn read_sta_ip_snapshot() -> Option<String> {
    use esp_idf_svc::sys::*;
    let ckey = std::ffi::CString::new("WIFI_STA_DEF").ok()?;
    let netif = unsafe { esp_netif_get_handle_from_ifkey(ckey.as_ptr()) };
    if netif.is_null() {
        return None;
    }
    let mut info: esp_netif_ip_info_t = unsafe { std::mem::zeroed() };
    if unsafe { esp_netif_get_ip_info(netif, &mut info) } != 0 {
        return None;
    }
    // ip.addr: u32 (LE byte order)
    let addr = info.ip.addr;
    Some(format!(
        "{}.{}.{}.{}",
        addr & 0xFF,
        (addr >> 8) & 0xFF,
        (addr >> 16) & 0xFF,
        (addr >> 24) & 0xFF
    ))
}

// =====================================================================
// SNTP / WiFi / OTA / Battery / Charge / Speed / Roster / Watchdog / helpers
// =====================================================================

/// Best-effort SNTP 初始化（ESP-IDF 自带 CONFIG_LWIP_SNTP=y 时已注册默认服务器）。
/// 这里只手动 set timezone 为 UTC + 触发一次；失败静默。
fn spawn_sntp_init_best_effort() {
    // 在 esp-idf-svc 0.51 中推荐的方式是直接用 `esp_idf_svc::sntp`；
    // 若符号不存在（不同 build 下 feature 有差异），改用 C sys 层的
    // sntp_setoperatingmode / sntp_init，两者都包在 unsafe block 中。
    std::thread::Builder::new()
        .name("sntp-init".into())
        .stack_size(4 * 1024)
        .spawn(|| {
            let _ = (); // SNTP 时间同步暂禁用（xtensa 编译缺 sntp symbols）
        })
        .ok();
}

// ---- 以下函数保持之前版本（略作格式整理） ----

/// AP 配置模式：把 ESP32 配成开放热点 `AstroBox-Setup`（192.168.4.1），
/// 供用户首次配置 WiFi。凭据为空时由 run_app 调用。
fn init_wifi_ap_mode(modem: Modem) -> anyhow::Result<BlockingWifi<EspWifi<'static>>> {
    use esp_idf_svc::wifi::AccessPointConfiguration;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), Some(nvs))?, sys_loop)?;
    let config = Configuration::AccessPoint(AccessPointConfiguration {
        ssid: "AstroBox-Setup"
            .try_into()
            .map_err(|_| anyhow!("AP SSID too long"))?,
        password: "".try_into().map_err(|_| anyhow!("AP password bad"))?,
        auth_method: AuthMethod::None,
        ..Default::default()
    });
    wifi.set_configuration(&config)?;
    wifi.start()?;
    wifi.wait_netif_up()?;
    log::info!("AP mode up: SSID=AstroBox-Setup, IP=192.168.4.1");
    Ok(wifi)
}

async fn init_wifi_with_retry(
    modem: Modem,
    ssid: &str,
    password: &str,
) -> anyhow::Result<BlockingWifi<EspWifi<'static>>> {
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), Some(nvs))?, sys_loop)?;

    let wifi_configuration = Configuration::Client(ClientConfiguration {
        ssid: ssid
            .try_into()
            .map_err(|_| anyhow!("Wi-Fi SSID is too long"))?,
        password: password
            .try_into()
            .map_err(|_| anyhow!("Wi-Fi password is too long"))?,
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    });

    wifi.set_configuration(&wifi_configuration)?;
    wifi.start()?;
    log::info!("Wi-Fi started");

    for attempt in 1..=WIFI_INIT_MAX_RETRIES {
        match wifi.connect() {
            Ok(()) => match wifi.wait_netif_up() {
                Ok(()) => {
                    log::info!("Wi-Fi connected to {ssid}");
                    return Ok(wifi);
                }
                Err(err) => {
                    log::warn!("Wi-Fi netif up failed on attempt {attempt}: {err:?}");
                }
            },
            Err(err) => {
                log::warn!(
                    "Wi-Fi connect attempt {attempt}/{WIFI_INIT_MAX_RETRIES} failed: {err:?}"
                );
            }
        }

        if attempt < WIFI_INIT_MAX_RETRIES {
            let delay = WIFI_INIT_RETRY_DELAY.saturating_mul(attempt as u32);
            log::info!("Retrying Wi-Fi connection in {:?}...", delay);
            tokio::time::sleep(delay).await;
        }
    }

    Err(anyhow!(
        "Wi-Fi connection failed after {WIFI_INIT_MAX_RETRIES} attempts"
    ))
}

async fn ota_check_loop(manager: std::sync::Arc<ota::OtaManager>) {
    let mut ticker = tokio::time::interval(OTA_CHECK_INTERVAL);
    loop {
        ticker.tick().await;
        if let Some(info) = manager.check_for_update() {
            log::debug!(
                "OTA update available: v{} ({} bytes, {}, url: {})",
                info.version,
                info.size,
                info.release_notes,
                info.url
            );
        }
    }
}

fn configure_component_log_levels() {
    let logger = EspLogger::new();

    if let Err(err) = logger.set_target_level("NimBLE", LevelFilter::Warn) {
        log::warn!("failed to set NimBLE log level: {err:?}");
    }

    if let Err(err) = logger.set_target_level(
        "corelib::device::xiaomi::components::network::native",
        LevelFilter::Warn,
    ) {
        log::warn!("failed to set network native log level: {err:?}");
    }
}

async fn log_network_meter() {
    let speeds = corelib::ecs::with_rt_mut(|rt| {
        let ids = rt.device_ids().cloned().collect::<Vec<_>>();
        let world = rt.world();

        ids.into_iter()
            .filter_map(|device_id| {
                let entity = rt.device_entity(&device_id)?;
                let dev = world.get::<XiaomiDevice>(entity)?;
                let name = dev.name().to_string();
                let addr = dev.addr().to_string();
                let speed = SpeedSnapshot::default();
                Some((name, addr, speed))
            })
            .collect::<Vec<_>>()
    })
    .await;

    if speeds.is_empty() {
        log::info!("NET meter: no connected devices");
        return;
    }

    for (name, addr, speed) in speeds {
        log::info!(
            "NET meter {name}({addr}) ↑{:.1} KB/s ↓{:.1} KB/s",
            speed.write / 1024.0,
            speed.read / 1024.0
        );
    }
}

#[derive(Clone, Copy, Default)]
struct SpeedSnapshot { write: f64, read: f64 }

#[derive(Clone)]
struct DeviceSnapshot {
    device_id: String,
    device_name: String,
    write_bps: f64,
    read_bps: f64,
}

async fn read_first_device_snapshot() -> Option<DeviceSnapshot> {
    corelib::ecs::with_rt_mut(|rt| {
        let device_id = rt.device_ids().next()?.to_string();
        let entity = rt.device_entity(&device_id)?;
        let world = rt.world();
        let dev = world.get::<XiaomiDevice>(entity)?;
        let _ = world.get::<NetworkComponent>(entity);
        let speed = SpeedSnapshot::default();
        Some(DeviceSnapshot {
            device_id,
            device_name: dev.name().to_string(),
            write_bps: speed.write,
            read_bps: speed.read,
        })
    })
    .await
}

async fn read_device_battery_status(device_id: &str) -> Option<(i32, String)> {
    let owner_id = device_id.to_string();
    let status_rx = corelib::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&owner_id, |world, entity| {
            let mut info = world.get_mut::<InfoSystem>(entity)?;
            Some(info.request_device_status())
        })
        .flatten()
    })
    .await?;

    let status = tokio::time::timeout(Duration::from_secs(2), status_rx)
        .await
        .ok()?
        .ok()?
        .ok()?;

    let battery = status.battery;
    let percent = battery.capacity.clamp(0, 100) as i32;
    let charge_text = format_charge_text(battery.charge_info.and_then(|info| info.timestamp));
    Some((percent, charge_text))
}

fn format_charge_text(charge_timestamp: Option<u32>) -> String {
    let Some(timestamp) = charge_timestamp else {
        return "充电信息未知".to_string();
    };
    let now_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => return "充电信息未知".to_string(),
    };
    let charge_secs = timestamp as u64;
    if now_secs <= charge_secs {
        return "刚刚充电".to_string();
    }
    let days = (now_secs - charge_secs) / 86_400;
    if days == 0 {
        "今天充电".to_string()
    } else {
        format!("{days} 天前充电")
    }
}

fn format_speed_text(speed_bps: f64, arrow: &str) -> String {
    let speed = speed_bps.max(0.0);
    if speed < 1024.0 {
        format!("{speed:.0} byte/s {arrow}")
    } else if speed < 1024.0 * 1024.0 {
        format!("{:.1} KB/s {arrow}", speed / 1024.0)
    } else {
        format!("{:.2} MB/s {arrow}", speed / (1024.0 * 1024.0))
    }
}

async fn log_device_roster() {
    let device_ids =
        corelib::ecs::with_rt_mut(|rt| rt.device_ids().cloned().collect::<Vec<_>>()).await;

    if device_ids.is_empty() {
        return;
    }

    for addr in &device_ids {
        match transfer::get_device_info(addr).await {
            Ok(name) => {
                log::info!("[Transfer] Device: {} ({})", name, addr);
            }
            Err(err) => {
                log::debug!("[Transfer] Failed to get name for {}: {err:?}", addr);
            }
        }
    }
}

async fn sync_installed_items() {
    let device_ids =
        corelib::ecs::with_rt_mut(|rt| rt.device_ids().cloned().collect::<Vec<_>>()).await;

    if device_ids.is_empty() {
        return;
    }

    for addr in &device_ids {
        match install::list_installed_watchfaces(addr).await {
            Ok(faces) => {
                log::info!(
                    "[Install] Device {} has {} watchface(s): {:?}",
                    addr,
                    faces.len(),
                    faces
                );
            }
            Err(err) => {
                log::debug!("[Install] Failed to list watchfaces on {}: {err:?}", addr);
            }
        }

        match install::list_installed_quick_apps(addr).await {
            Ok(apps) => {
                log::info!(
                    "[Install] Device {} has {} quick app(s): {:?}",
                    addr,
                    apps.len(),
                    apps
                );
            }
            Err(err) => {
                log::debug!("[Install] Failed to list quick apps on {}: {err:?}", addr);
            }
        }
    }
}

async fn wifi_reconnect_watchdog(
    wifi: BlockingWifi<EspWifi<'static>>,
    ssid: String,
    password: String,
) {
    enum WifiCmd {
        CheckAndReconnect {
            reply: tokio::sync::oneshot::Sender<()>,
        },
    }

    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<WifiCmd>(4);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();

    let _wifi_thread = std::thread::Builder::new()
        .name("wifi-wd".into())
        .stack_size(16 * 1024)
        .spawn(move || {
            let mut wifi = wifi;
            let mut last_disconnected_snapshot = false;
            let poll_interval = std::time::Duration::from_millis(500);
            loop {
                if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        WifiCmd::CheckAndReconnect { reply } => {
                            let _ = wifi_reconnect_blocking(&mut wifi, &ssid, &password);
                            let _ = reply.send(());
                        }
                    }
                }
                let connected = wifi.is_connected().unwrap_or(false);
                if !connected && !last_disconnected_snapshot {
                    log::warn!("[Wifi-Watchdog] link lost on worker thread; reconnecting...");
                }
                last_disconnected_snapshot = connected;
                if !connected {
                    let _ = wifi_reconnect_blocking(&mut wifi, &ssid, &password);
                }
                // === webui: refresh static WIFI_CONNECTED + WIFI_STA_IP every tick ===
                WIFI_CONNECTED.store(connected, std::sync::atomic::Ordering::Relaxed);
                // STA IP：通过 EspWifi 的 netif 查询。
                // （若未来 EspWifi 不可用，退化为不更新 IP — 不会打断 WiFi 重连。）
                if connected {
                    let ip = read_sta_ip_snapshot();
                    if let Some(ip) = ip {
                        if let Ok(mut g) = WIFI_STA_IP.get_or_init(|| std::sync::RwLock::new(String::new())).write() {
                            if *g != ip {
                                *g = ip;
                            }
                        }
                    }
                }
                std::thread::sleep(poll_interval);
            }
        })
        .expect("spawn wifi watchdog worker thread");

    let mut ticker = tokio::time::interval(WIFI_RECONNECT_CHECK_INTERVAL);
    loop {
        ticker.tick().await;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        match cmd_tx
            .send(WifiCmd::CheckAndReconnect { reply: reply_tx })
            .await
        {
            Ok(()) => {
                let _ = reply_rx.await;
            }
            Err(_closed) => {
                log::warn!("[Wifi-Watchdog] worker thread exited; watchdog disabled");
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    }
}

fn wifi_reconnect_blocking(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    ssid: &str,
    password: &str,
) -> Result<(), anyhow::Error> {
    if wifi.is_connected().unwrap_or(false) {
        return Ok(());
    }
    let _ = wifi.disconnect();
    wifi.connect().map_err(|e| anyhow!("connect: {e:?}"))?;
    wifi.wait_netif_up()
        .map_err(|e| anyhow!("wait_netif_up: {e:?}"))?;
    log::info!("Wi-Fi reconnected to {ssid}");
    if let Ok(()) = nvs_config::save_wifi_credentials(ssid, password) {
        log::debug!("Wi-Fi credentials saved to NVS");
    }
    Ok(())
}

// =====================================================================
// 对外的"主机侧 API"（给未来的 RPC / App 调用）
// =====================================================================

pub async fn install_quick_app_on_device(
    addr: &str,
    package_name: &str,
    data: Vec<u8>,
) -> anyhow::Result<()> {
    install::install_quick_app(addr, package_name, data).await
}

pub async fn install_quick_app_file_on_device(
    addr: &str,
    package_name: &str,
    file_path: &str,
) -> anyhow::Result<()> {
    install::install_quick_app_from_file(addr, package_name, file_path).await
}

pub async fn install_watchface_on_device(addr: &str, data: Vec<u8>) -> anyhow::Result<()> {
    install::install_watchface(addr, data).await
}

pub async fn install_watchface_file_on_device(addr: &str, file_path: &str) -> anyhow::Result<()> {
    install::install_watchface_from_file(addr, file_path).await
}

pub async fn uninstall_quick_app_on_device(addr: &str, package_name: &str) -> anyhow::Result<()> {
    install::uninstall_quick_app(addr, package_name).await
}

pub async fn uninstall_watchface_on_device(addr: &str, watchface_id: &str) -> anyhow::Result<()> {
    install::uninstall_watchface(addr, watchface_id).await
}

pub async fn set_watchface_on_device(addr: &str, watchface_id: &str) -> anyhow::Result<()> {
    install::set_watchface(addr, watchface_id).await
}

pub async fn launch_quick_app_on_device(addr: &str, package_name: &str) -> anyhow::Result<()> {
    install::launch_quick_app(addr, package_name).await
}

// ===== Transfer module public API =====

pub async fn send_data_to_device(
    addr: &str,
    data_type: corelib::device::xiaomi::packet::mass::MassDataType,
    data: Vec<u8>,
) -> anyhow::Result<()> {
    transfer::send_data_to_device(addr, data_type, data).await
}

pub async fn forward_app_message_between_devices(
    src_addr: &str,
    dst_addr: &str,
    package_name: &str,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    transfer::forward_app_message(src_addr, dst_addr, package_name, payload).await
}

pub async fn relay_interconnect_between_devices(
    src_addr: &str,
    dst_addr: &str,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    transfer::relay_interconnect_message(src_addr, dst_addr).await
}

pub async fn copy_quick_app_between_devices(
    src_addr: &str,
    dst_addr: &str,
    package_name: &str,
) -> anyhow::Result<()> {
    transfer::transfer_quick_app_between_devices(src_addr, dst_addr, package_name).await
}

pub async fn copy_watchface_between_devices(
    src_addr: &str,
    dst_addr: &str,
    watchface_id: &str,
) -> anyhow::Result<()> {
    transfer::transfer_watchface_between_devices(src_addr, dst_addr, watchface_id).await
}

pub async fn broadcast_data_to_all_devices(
    data_type: corelib::device::xiaomi::packet::mass::MassDataType,
    data: Vec<u8>,
) -> anyhow::Result<Vec<(String, anyhow::Result<()>)>> {
    transfer::broadcast_data_to_all_devices(data_type, data).await
}

pub async fn list_connected_devices() -> Vec<String> {
    transfer::list_connected_devices().await
}

pub async fn get_device_name(addr: &str) -> anyhow::Result<String> {
    transfer::get_device_info(addr).await
}
