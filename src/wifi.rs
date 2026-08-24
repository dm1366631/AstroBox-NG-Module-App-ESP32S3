//! WiFi 连接管理（STA 模式）。
//!
//! 连接到指定的 WiFi 路由器，获取 IP 地址后即可访问 Web 管理界面。
//! 修改 WIFI_SSID / WIFI_PASS 为你的路由器凭据。

use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    nvs::EspDefaultNvsPartition,
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi},
};
// NVS is at esp_idf_svc::nvs, not esp_idf_svc::hal::nvs
use log::info;

/// WiFi SSID — 修改为你的路由器名称。
pub const WIFI_SSID: &str = "YOUR_WIFI_SSID";
/// WiFi 密码 — 修改为你的路由器密码。
pub const WIFI_PASS: &str = "YOUR_WIFI_PASSWORD";

/// 连接 WiFi，返回已连接的 Wifi 实例（需保持存活）。
pub fn connect_wifi(
    modem: esp_idf_svc::hal::modem::Modem,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<BlockingWifi<EspWifi<'static>>, Box<dyn std::error::Error>> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop)?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASS.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;

    info!("Connecting to WiFi: {}...", WIFI_SSID);
    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("WiFi connected! IP: {}", ip_info.ip);
    info!("Web UI: http://{}/", ip_info.ip);

    Ok(wifi)
}
