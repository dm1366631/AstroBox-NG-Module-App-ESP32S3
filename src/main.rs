//! AstroBox-NG Module firmware — MINIMAL + Web 管理版.
//!
//! 功能：ESP32-S3 init + SPI2 + ST7789 LCD + WiFi + 快应用/表盘管理 Web 控制台。
//! 修改 src/wifi.rs 里的 WIFI_SSID / WIFI_PASS 为你的路由器凭据。

mod abp_package;
mod package_manager;
mod web_server;
mod wifi;

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::Text,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        delay::Delay,
        gpio::PinDriver,
        ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver, LEDC},
        modem::Modem,
        prelude::Peripherals,
        spi::{SpiConfig, SpiDeviceDriver, SpiDriver},
    },
    log::EspLogger,
    nvs::EspDefaultNvsPartition,
    sys::link_patches,
};
use mipidsi::{
    interface::SpiInterface,
    models::ST7789,
    options::{ColorInversion, ColorOrder, Orientation, RefreshOrder},
    Builder,
};
use package_manager::PackageManager;

const DISPLAY_SPI_BUFFER_SIZE: usize = 4096;
static mut DISPLAY_SPI_BUFFER: [u8; DISPLAY_SPI_BUFFER_SIZE] = [0; DISPLAY_SPI_BUFFER_SIZE];

fn main() {
    link_patches();
    EspLogger::initialize_default();
    log::set_max_level(log::LevelFilter::Info);

    log::info!("AstroBox-NG MINIMAL+Web firmware starting...");

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;
    let spi2 = peripherals.spi2;
    let ledc = peripherals.ledc;
    let modem = unsafe { Modem::new() };

    // ===== NVS + 事件循环（WiFi 需要）=====
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();

    // ===== SPI2 bus: SCLK=GPIO7, MOSI=GPIO6, MISO=GPIO8 =====
    let spi_driver = SpiDriver::new(
        spi2,
        pins.gpio7,
        pins.gpio6,
        Some(pins.gpio8),
        &esp_idf_svc::hal::spi::SpiDriverConfig::new()
            .dma(esp_idf_svc::hal::spi::Dma::Auto(DISPLAY_SPI_BUFFER_SIZE)),
    )
    .unwrap();

    // ===== LCD: CS=GPIO5, DC=GPIO4, RST=GPIO3, BL=GPIO2 =====
    let dc = PinDriver::output(pins.gpio4).unwrap();
    let rst = PinDriver::output(pins.gpio3).unwrap();

    let LEDC { timer0, channel0, .. } = ledc;
    let ledc_timer =
        LedcTimerDriver::new(timer0, &TimerConfig::new().frequency(25_000.into())).unwrap();
    let mut backlight = LedcDriver::new(channel0, ledc_timer, pins.gpio2).unwrap();
    backlight.set_duty(backlight.get_max_duty() / 2).unwrap();

    let spi_dev = SpiDeviceDriver::new(
        &spi_driver,
        Some(pins.gpio5),
        &SpiConfig::new().baudrate(40_000_000.into()),
    )
    .unwrap();

    #[allow(static_mut_refs)]
    let buffer: &'static mut [u8] = unsafe { &mut DISPLAY_SPI_BUFFER };
    let di = SpiInterface::new(spi_dev, dc, buffer);

    let mut delay = Delay::new_default();
    let mut display = Builder::new(ST7789, di)
        .reset_pin(rst)
        .invert_colors(ColorInversion::Normal)
        .color_order(ColorOrder::Bgr)
        .orientation(Orientation::new().rotate(mipidsi::options::Rotation::Deg0))
        .refresh_order(RefreshOrder::new(
            mipidsi::options::VerticalRefreshOrder::TopToBottom,
            mipidsi::options::HorizontalRefreshOrder::LeftToRight,
        ))
        .display_size(240, 320)
        .display_offset(0, 0)
        .init(&mut delay)
        .unwrap();

    log::info!("Display initialized: 240x320 ST7789");

    // ===== 包管理器 =====
    let pkg_manager = PackageManager::new();

    // ===== 连接 WiFi =====
    let _wifi = match wifi::connect_wifi(modem, sysloop, nvs) {
        Ok(w) => {
            let ip_info = w.wifi().sta_netif().get_ip_info().unwrap();
            draw_screen(&mut display, &ip_info.ip.to_string(), pkg_manager.count());
            Some(w)
        }
        Err(e) => {
            log::error!("WiFi connect failed: {e}");
            draw_screen(&mut display, "WiFi FAIL", 0);
            None
        }
    };

    // ===== 启动 Web 服务器 =====
    if _wifi.is_some() {
        match web_server::start_server(pkg_manager.clone()) {
            Ok(_server) => {
                log::info!("Web server running");
            }
            Err(e) => {
                log::error!("Web server start failed: {e}");
            }
        }
    }

    log::info!("System ready. Entering main loop.");

    // ===== 主循环：保持设备存活 =====
    loop {
        std::thread::sleep(std::time::Duration::from_millis(5000));
        log::debug!("alive, packages: {}", pkg_manager.count());
    }
}

/// 在屏幕上显示状态信息。
fn draw_screen<D>(display: &mut D, ip: &str, pkg_count: usize)
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
{
    display.clear(Rgb565::BLACK).unwrap();

    // 标题栏
    Rectangle::new(Point::new(0, 0), Size::new(240, 36))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_BLUE))
        .draw(display)
        .unwrap();

    let title_style = MonoTextStyle::new(&FONT_6X10, Rgb565::WHITE);
    Text::new("AstroBox-NG", Point::new(8, 22), title_style)
        .draw(display)
        .unwrap();

    let body_style = MonoTextStyle::new(&FONT_6X10, Rgb565::YELLOW);
    Text::new("Web Manager", Point::new(8, 55), body_style)
        .draw(display)
        .unwrap();

    let ip_style = MonoTextStyle::new(&FONT_6X10, Rgb565::GREEN);
    Text::new(&format!("IP: {}", ip), Point::new(8, 75), ip_style)
        .draw(display)
        .unwrap();

    let info_style = MonoTextStyle::new(&FONT_6X10, Rgb565::CSS_LIGHT_GRAY);
    Text::new(&format!("Packages: {}", pkg_count), Point::new(8, 95), info_style)
        .draw(display)
        .unwrap();
    Text::new("Open IP in browser", Point::new(8, 115), info_style)
        .draw(display)
        .unwrap();
    Text::new("to install .abp files", Point::new(8, 130), info_style)
        .draw(display)
        .unwrap();
}
