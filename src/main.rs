//! AstroBox-NG Module firmware — MINIMAL build.
//!
//! Only the bare essentials: ESP32-S3 init + SPI2 + ST7789 LCD +
//! embedded-graphics demo. No Wi-Fi, BLE, SD card, slint, or network.

use esp_idf_svc::{
    hal::{
        delay::Delay,
        gpio::PinDriver,
        ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver, LEDC},
        modem::Modem,
        prelude::Peripherals,
        spi::{SpiConfig, SpiDeviceDriver, SpiDriver},
    },
    log::EspLogger,
    sys::link_patches,
};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Circle, PrimitiveStyle, Rectangle},
    text::Text,
};
use mipidsi::{
    interface::SpiInterface,
    models::ST7789,
    options::{ColorInversion, ColorOrder, Orientation, RefreshOrder},
    Builder,
};

const DISPLAY_SPI_BUFFER_SIZE: usize = 4096;
static mut DISPLAY_SPI_BUFFER: [u8; DISPLAY_SPI_BUFFER_SIZE] = [0; DISPLAY_SPI_BUFFER_SIZE];

fn main() {
    link_patches();
    EspLogger::initialize_default();
    log::set_max_level(log::LevelFilter::Info);

    log::info!("AstroBox-NG MINIMAL firmware starting...");

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;
    let spi2 = peripherals.spi2;
    let ledc = peripherals.ledc;
    let _modem = unsafe { Modem::new() };

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

    // ===== Draw demo screen =====
    display.clear(Rgb565::BLACK).unwrap();

    Rectangle::new(Point::new(0, 0), Size::new(240, 40))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_BLUE))
        .draw(&mut display)
        .unwrap();

    let title_style = MonoTextStyle::new(&FONT_6X10, Rgb565::WHITE);
    Text::new("AstroBox-NG", Point::new(10, 25), title_style)
        .draw(&mut display)
        .unwrap();

    Circle::new(Point::new(80, 100), 80)
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::GREEN, 3))
        .draw(&mut display)
        .unwrap();

    let body_style = MonoTextStyle::new(&FONT_6X10, Rgb565::YELLOW);
    Text::new("MINIMAL build OK", Point::new(10, 220), body_style)
        .draw(&mut display)
        .unwrap();
    Text::new("ESP32-S3 + ST7789", Point::new(10, 240), body_style)
        .draw(&mut display)
        .unwrap();
    Text::new("No WiFi/BLE/SD/slint", Point::new(10, 260), body_style)
        .draw(&mut display)
        .unwrap();

    log::info!("Demo screen drawn");

    loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
}
