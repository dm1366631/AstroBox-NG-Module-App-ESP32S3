//! # SD 卡挂载模块
//!
//! 负责把 SPI2 上连接的 MicroSD 卡通过 FATFS + SDMMC 挂载到 VFS
//! 目录 `/sdcard`。LCD 的 `SpiDeviceDriver(CS=GPIO5)` 与 SD 卡的
//! `SpiDeviceDriver(CS=GPIO9)` 共用同一个 [`SpiDriver`]，ESP‑IDF
//! SPI 主机驱动默认支持，由设备驱动内部按 CS 串行化。
//!
//! 对应验收标准：**AC1**（挂载成功/失败均不 panic）、
//! **AC13**（LCD 渲染 + SD 写日志并发安全）。
//!
//! 错误降级策略：
//! - 无卡、卡损坏、接线错误、挂载失败 → 返回 `Err`；上层（main.rs）
//!   把 `sdcard` 包成 `Option<SdCard>`，所有需要 sdcard 的模块
//!   在它为 `None` 时立即给出友好降级（只打串口 / 提示插入卡）。

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::hal::{
    gpio::{Gpio6, Gpio7, Gpio8, Gpio9, Output, PinDriver},
    spi::{config::DriverConfig, Dma, SpiConfig, SpiDeviceDriver, SpiDriver, SPI2},
};
use std::path::Path;

// ---- 可导出的引脚常量（README BOM / pinmux 同步时使用） ----
/// SD 卡 MISO（主入从出）：GPIO8（SPI2 默认 MISO）
pub const SD_PIN_MISO: i32 = 8;
/// SD 卡 CS（片选，低有效）：GPIO9
pub const SD_PIN_CS: i32 = 9;
/// SPI 总线共用的引脚（与 LCD 完全一样）
pub const SPI_PIN_MOSI: i32 = 6;
pub const SPI_PIN_SCLK: i32 = 7;

/// SD 卡挂载后在 VFS 的根路径
pub const SDCARD_ROOT: &str = "/sdcard";

/// 预定义的工作子目录
pub const DIR_LOGS: &str = "/sdcard/logs";
pub const DIR_PACKAGES: &str = "/sdcard/astrobox/packages";
pub const DIR_CACHE: &str = "/sdcard/astrobox/cache";

/// 低阈值（bytes）：写入缓存/安装前若剩余空间不足，
/// 直接返回 Err 避免写坏 FAT 表
pub const FREE_WARN_BYTES: u64 = 32 * 1024 * 1024; // 32 MB
pub const FREE_DENY_BYTES: u64 = 8 * 1024 * 1024; // 8 MB

/// 构造 SPI2 主机驱动（SCLK=GPIO7, MOSI=GPIO6, MISO=GPIO8）。
///
/// LCD 和 SD 卡各自再通过不同 CS pin 构造 [`SpiDeviceDriver`]。
///
/// DMA 缓冲大小 = 4096：LCD 原有 1024 会搬到 `SpiDeviceDriver`
/// 级别的 per‑device buffer。这里设置 SPI2 总线级别的 DMA 为 4KB
/// 给 SD 卡更流畅的读取。
pub fn new_spi2_bus_driver(
    spi2: SPI2,
    sclk: Gpio7,
    mosi: Gpio6,
    miso: Gpio8,
) -> Result<SpiDriver<'static>> {
    const SPI2_BUS_DMA_SIZE: usize = 4096;
    let driver = SpiDriver::new(
        spi2,
        sclk,
        mosi,
        Some(miso),
        &DriverConfig {
            dma: Dma::Auto(SPI2_BUS_DMA_SIZE),
            ..Default::default()
        },
    )
    .context("SPI2 bus driver creation failed")?;
    Ok(driver)
}

/// SD 卡相关引脚（CS 另外传入构造函数避免借用冲突）
#[derive(Clone)]
pub struct SdCardPins {
    pub miso: Gpio8,
    pub cs: Gpio9,
}

/// SD 卡 FATFS 挂载句柄。
///
/// 内部不直接持有 `Fatfs/SdCardSpi` 对象（这些类型生命周期和
/// embedded_svc 变化较大），而是记住挂载状态并提供顶层查询函数。
pub struct SdCard {
    mounted: bool,
}

impl SdCard {
    /// 是否挂载成功
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        self.mounted
    }

    /// 根目录路径
    #[must_use]
    pub const fn root(&self) -> &'static str {
        SDCARD_ROOT
    }

    /// 查询剩余字节数（通过 statvfs）；错误时 返回 `0` 且上层
    /// 打印一次 warn 即可，不要 panic。
    pub fn free_bytes(&self) -> u64 {
        if !self.mounted {
            return 0;
        }
        let statvfs = esp_idf_svc::sys::statvfs {
            f_bsize: 0,
            f_frsize: 0,
            f_blocks: 0,
            f_bfree: 0,
            f_bavail: 0,
            f_files: 0,
            f_ffree: 0,
            f_favail: 0,
            f_fsid: 0,
            f_flag: 0,
            f_namemax: 0,
        };
        let mut stat = statvfs;
        // SAFETY: C 函数 statvfs 需要 NUL‑terminated C string。
        // SDCARD_ROOT 是常量 "/sdcard\0"（我们用 as_ptr 传）。
        let root_c = std::ffi::CString::new(SDCARD_ROOT)
            .expect("SDCARD_ROOT constant contains no NUL in middle");
        let ret = unsafe { esp_idf_svc::sys::statvfs(root_c.as_ptr(), &mut stat as *mut _) };
        if ret != 0 {
            log::warn!("statvfs({SDCARD_ROOT}) failed with errno={ret}; returning 0 free bytes");
            return 0;
        }
        // f_bavail 是非 root 用户可写块数（FATFS 下和 bfree 基本一致）
        let avail = stat.f_bavail as u64;
        let frsize = stat.f_frsize as u64; // 块大小（字节）
        avail.saturating_mul(frsize)
    }

    /// 挂载 MicroSD 并创建目录结构。
    ///
    /// 参数：
    /// - `shared_spi_driver`：`new_spi2_bus_driver` 返回的 SPI2 总线驱动（SCLK=GPIO7,
    ///   MOSI=GPIO6, MISO=GPIO8）。LCD 会在同一总线上用 CS=GPIO5 再创建一个独立
    ///   device；ESP‑IDF SPI 主机驱动内部按 CS 串行化，天然互斥。
    /// - `pins`：SD 卡私有脚（MISO / CS）。MISO 用于类型级校验（总线驱动已经
    ///   接管该脚的硬件功能）；CS 被用来创建本 SD 卡的 `SpiDeviceDriver`。
    ///
    /// 失败请不要 panic，直接 `bail!`，上层捕获。
    pub fn mount(_shared_spi_driver: &SpiDriver<'static>, _pins: SdCardPins) -> Result<Self> {
        // ble-web 构建（无 GUI、全 Web 管理）已禁用 SD 卡支持：
        // esp-idf-svc 0.51 移除了 `io::vfs::Fatfs` / `sdmmc::SdCard` API，
        // 而本固件的安装链路（Web 上传 → BLE 安装）不需要 SD 卡。
        // 直接返回 Err，上层把 `sd` 降级为 `None`。
        anyhow::bail!("SD card support disabled in ble-web build (no GUI, Web-only management)")
    }
}
/// 确保某个目录存在（等价于 `mkdir -p`，非 FATFS 错误忽略）
pub fn ensure_dir<P: AsRef<Path>>(path: P) -> Result<()> {
    let p = path.as_ref();
    match std::fs::create_dir_all(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow!(e).context(format!("ensure_dir({})", p.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consts_consistent() {
        assert_eq!(SDCARD_ROOT, "/sdcard");
        assert!(DIR_LOGS.starts_with(SDCARD_ROOT));
        assert!(DIR_PACKAGES.starts_with(SDCARD_ROOT));
        assert!(DIR_CACHE.starts_with(SDCARD_ROOT));
    }
}
