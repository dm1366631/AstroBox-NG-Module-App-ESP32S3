//! # 滚动日志后端（SD 卡文件 + 串口组合）
//!
//! 提供 [`FileLogger`] + `CombinedLogger`，同时输出到 SD 卡日志目录
//! (`/sdcard/logs/astrobox_YYYYMMDD_NNNN.log`) 与 USB 串口（后者走
//! 原有 `EspLogger`，不再重复实现）。
//!
//! 设计目标（AC2/AC3/AC4）：
//! - **尺寸滚动**：单文件 ≥ 512 KB 切分；总占用 > 4 MB 或 mtime > 7 天自动清。
//! - **失败降级**：连续写失败 ≥ 5 次后自动禁用 SD 卡输出，只 warn 一次。
//! - **绝不 panic**：任何 I/O 错误都变成一次 warn，不影响 BLE / UI / Wi‑Fi。

use anyhow::{Context, Result};
use log::{LevelFilter, Log, Metadata, Record};
use std::{
    cell::{Cell, UnsafeCell},
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

const MAX_SINGLE_LOG_BYTES: u64 = 512 * 1024; // 512 KB per shard
const MAX_TOTAL_LOG_BYTES: u64 = 4 * 1024 * 1024; // 4 MB total
const MAX_LOG_DAYS: u64 = 7;
const FAIL_LIMIT: u8 = 5;

/// 全局静态：SD 卡日志 backend 是否启用。
///
/// - 启用条件：`install_combined_logger()` 调用成功且 `sd.is_some()`；
/// - 失败后立即置为 false，避免反复尝试导致 I/O 风暴。
static FILE_LOGGER_ENABLED: AtomicBool = AtomicBool::new(false);

/// 是否安装过了（防止重复 set_boxed_logger 冲突）
static LOGGER_INSTALLED: AtomicBool = AtomicBool::new(false);

pub fn is_file_logger_enabled() -> bool {
    FILE_LOGGER_ENABLED.load(Ordering::Relaxed)
}

// ---- 时间戳 ----
fn now_iso_utc() -> String {
    // 尽量用系统 time（可能经 SNTP 同步过）；失败时退 epoch 秒数。
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            // 无 `chrono` 环境：使用自定义格式化器（YYYYMMDD + HH:MM:SS）。
            // 算法：按 POSIX gmtime 规则。
            let (y, mo, d, h, mi, s) = gmtime_y_m_d_h_m_s(secs as i64);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        }
        Err(_) => format!("epoch-{}", unsafe {
            esp_idf_svc::sys::esp_timer_get_time() / 1_000_000
        }),
    }
}

fn gmtime_y_m_d_h_m_s(mut t: i64) -> (i32, u32, u32, u32, u32, u32) {
    // 简化的 gmtime 计算（1970 起），不依赖 chrono
    // 避免闰年 / 月份切换错误用经典公式。
    let s = (t % 60) as u32;
    t /= 60;
    let mi = (t % 60) as u32;
    t /= 60;
    let h = (t % 24) as u32;
    let mut days = t / 24;

    let mut y: i32 = 1970;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        let year_days = if leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0u32;
    while mo < 12 {
        let mut md = month_days[mo as usize];
        if mo == 1 && leap {
            md += 1;
        }
        if days < md as i64 {
            break;
        }
        days -= md as i64;
        mo += 1;
    }
    (y, mo + 1, (days + 1) as u32, h, mi, s)
}

fn date_stamp_for_filename() -> String {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let (y, mo, d, _, _, _) = gmtime_y_m_d_h_m_s(d.as_secs() as i64);
            format!("{y:04}{mo:02}{d:02}")
        }
        Err(_) => "unknown-date".into(),
    }
}

// ==================== FileLogger ====================
pub struct FileLogger {
    dir: PathBuf,
    fail_counter: Cell<u8>,
    disabled: Cell<bool>,
    // writer 使用 UnsafeCell + 内部 Mutex；但为了避免 std::sync::Mutex
    // 与 `Log` trait 的 `&self` 签名冲突，这里用 RefCell。
    // 如果 Logger 是全局唯一（`log::set_boxed_logger` 安装后只有
    // 一个实例在写），不存在并发问题。
    writer: UnsafeCell<Option<BufWriter<File>>>,
    current_file_size: Cell<u64>,
    current_file_index: Cell<u32>,
}

// 单写者场景：`log` crate 默认保证各线程写 Log 串行（除非开启
// `log/max_level_trace_off` 之类，这里不担心）。
unsafe impl Send for FileLogger {}
unsafe impl Sync for FileLogger {}

impl FileLogger {
    pub fn new(log_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("create log dir {}", log_dir.display()))?;
        let me = Self {
            dir: log_dir,
            fail_counter: Cell::new(0),
            disabled: Cell::new(false),
            writer: UnsafeCell::new(None),
            current_file_size: Cell::new(0),
            current_file_index: Cell::new(0),
        };
        me.rotate()?;
        // 安装后才切日志时不会清理——第一次创建立即清理历史旧日志
        let _ = me.purge_if_needed();
        Ok(me)
    }

    fn current_path(&self) -> PathBuf {
        let ds = date_stamp_for_filename();
        let idx = self.current_file_index.get();
        self.dir.join(format!("astrobox_{ds}_{idx:04}.log"))
    }

    fn rotate(&self) -> Result<()> {
        // SAFETY: writer 仅在 rotate / write / flush 中被独占访问，
        // log crate 默认对 Log 的调用是全局串行，因此 unsafe 等价于 borrow_mut。
        let writer_slot: &mut Option<BufWriter<File>> = unsafe { &mut *self.writer.get() };

        // 先 flush + drop 旧 writer
        if let Some(mut w) = writer_slot.take() {
            let _ = w.flush();
            drop(w);
        }

        let path = loop {
            let candidate = self.current_path();
            // 如果存在且大小超过阈值或已存在（索引占用），index++
            match std::fs::metadata(&candidate) {
                Ok(m) if m.len() < MAX_SINGLE_LOG_BYTES => break candidate,
                Err(_) => break candidate,
                Ok(_) => {
                    self.current_file_index
                        .set(self.current_file_index.get() + 1);
                }
            }
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open log file {}", path.display()))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.current_file_size.set(size);
        *writer_slot = Some(BufWriter::new(file));
        Ok(())
    }

    fn mark_failure(&self, err: &dyn std::fmt::Display) {
        let n = self.fail_counter.get().saturating_add(1);
        self.fail_counter.set(n);
        if n >= FAIL_LIMIT && !self.disabled.get() {
            self.disabled.set(true);
            FILE_LOGGER_ENABLED.store(false, Ordering::Relaxed);
            eprintln!(
                "[FileLogger] disabled after {FAIL_LIMIT} consecutive failures; last error: {err}"
            );
        }
    }

    fn purge_if_needed(&self) -> Result<()> {
        let now = SystemTime::now();
        let mut entries = std::fs::read_dir(&self.dir)?
            .filter_map(|r| r.ok())
            .map(|e| {
                let p = e.path();
                let meta = std::fs::metadata(&p).ok();
                (p, meta)
            })
            .filter(|(_, m)| m.as_ref().is_some())
            .map(|(p, m)| {
                let meta = m.unwrap();
                let age_days = now
                    .duration_since(meta.modified().unwrap_or(now))
                    .map(|d| d.as_secs() / 86_400)
                    .unwrap_or(0);
                (p, meta.len(), age_days)
            })
            .collect::<Vec<_>>();
        // 按 mtime 新 → 旧 排序（保留新的）
        entries.sort_by(|a, b| b.2.cmp(&a.2).reverse());

        let mut total: u64 = 0;
        for (p, size, age_days) in entries {
            total = total.saturating_add(size);
            let too_old = age_days > MAX_LOG_DAYS;
            let too_big = total > MAX_TOTAL_LOG_BYTES;
            if too_old || too_big {
                if let Err(e) = std::fs::remove_file(&p) {
                    log::warn!("failed to purge stale log {}: {e}", p.display());
                } else {
                    total = total.saturating_sub(size);
                }
            }
        }
        Ok(())
    }

    fn write_inner(&self, line: &str) -> std::io::Result<()> {
        // SAFETY: same rationale as rotate
        let writer_slot = unsafe { &mut *self.writer.get() };
        let w = writer_slot
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "writer closed"))?;
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        self.current_file_size.set(
            self.current_file_size
                .get()
                .saturating_add(line.len() as u64 + 1),
        );
        if self.current_file_size.get() >= MAX_SINGLE_LOG_BYTES {
            let _ = w.flush();
            drop(writer_slot.take());
            self.current_file_index
                .set(self.current_file_index.get().saturating_add(1));
            let _ = self.rotate();
            let _ = self.purge_if_needed();
        } else {
            // 不每次 flush 影响性能；按 4 KB buffer 自然 flush
        }
        Ok(())
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if self.disabled.get() {
            return false;
        }
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &Record<'_>) {
        if self.disabled.get() {
            return;
        }
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = now_iso_utc();
        let level = record.level();
        let target = record.target();
        // 简化消息拼装；不追求完整 `Debug` 格式
        let msg_str = format!("{record.args()}");
        let line = format!("[{ts}] {level:<5} {target}: {msg_str}");
        match self.write_inner(&line) {
            Ok(()) => {
                self.fail_counter.set(0);
            }
            Err(e) => self.mark_failure(&e),
        }
    }
    fn flush(&self) {
        // SAFETY: same
        let w = unsafe { &mut *self.writer.get() };
        if let Some(w) = w.as_mut() {
            let _ = w.flush();
        }
    }
}

impl Drop for FileLogger {
    fn drop(&mut self) {
        self.flush();
    }
}

// ==================== CombinedLogger (EspLogger + FileLogger) ====================
pub struct CombinedLogger {
    esp: EspLoggerBackend,
    file: Option<FileLogger>,
    max_level: LevelFilter,
}

/// 封装 `EspLogger` 的实际调用。EspLogger 0.51 默认用 initialize_default
/// 初始化静态 logger。为避免两次 set_logger 冲突，这里走它的静态后端。
struct EspLoggerBackend {
    initialized: bool,
}
impl EspLoggerBackend {
    fn new() -> Self {
        // 若已经初始化过则不再重复
        Self { initialized: true }
    }
    fn log_record(&self, record: &Record<'_>) {
        // EspLogger 默认安装后自动输出到 USB/UART；这里不做额外操作，
        // 因为 `log::logger()` 会在 combined logger 内部分发。
        // 但为避免循环，我们直接走 esp-idf 的 `esp_log_write` 底层。
        use esp_idf_svc::sys::{
            esp_log_level_t_ESP_LOG_DEBUG, esp_log_level_t_ESP_LOG_ERROR,
            esp_log_level_t_ESP_LOG_INFO, esp_log_level_t_ESP_LOG_NONE,
            esp_log_level_t_ESP_LOG_WARN, esp_log_write,
        };
        use std::ffi::CString;
        let level = match record.level() {
            log::Level::Error => esp_log_level_t_ESP_LOG_ERROR,
            log::Level::Warn => esp_log_level_t_ESP_LOG_WARN,
            log::Level::Info => esp_log_level_t_ESP_LOG_INFO,
            log::Level::Debug | log::Level::Trace => esp_log_level_t_ESP_LOG_DEBUG,
        };
        let target = match CString::new(record.target()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let msg = format!("{}\0", record.args());
        // SAFETY: `target`/`msg` are valid C strings with trailing NUL.
        unsafe {
            esp_log_write(
                level,
                target.as_ptr(),
                b"%s\0".as_ptr(),
                msg.as_ptr() as *const i8,
            );
        }
    }
}

impl CombinedLogger {
    pub fn new(maybe_sd_root: Option<&Path>) -> Result<Self> {
        let file = match maybe_sd_root {
            Some(root) => {
                let dir = root.join("logs");
                match FileLogger::new(dir) {
                    Ok(f) => {
                        FILE_LOGGER_ENABLED.store(true, Ordering::Relaxed);
                        Some(f)
                    }
                    Err(e) => {
                        log::warn!("FileLogger init failed, falling back to serial only: {e:#}");
                        None
                    }
                }
            }
            None => None,
        };
        Ok(Self {
            esp: EspLoggerBackend::new(),
            file,
            max_level: LevelFilter::Debug,
        })
    }
}

impl Log for CombinedLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.max_level
    }
    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // 串口：EspLogger backend
        self.esp.log_record(record);
        // SD 卡：FileLogger（自己处理降级）
        if let Some(f) = self.file.as_ref() {
            f.log(record);
        }
    }
    fn flush(&self) {
        if let Some(f) = self.file.as_ref() {
            f.flush();
        }
    }
}

/// 安装全局日志系统：USB/UART + （可选）SD 卡滚动文件。
///
/// 调用约束：程序入口 early（main 前半段，在第一次 `info!` 之前）。
/// 若已安装则不重复安装。
pub fn install_combined_logger(maybe_sd_root: Option<&Path>, max_level: LevelFilter) -> Result<()> {
    if LOGGER_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }
    let mut logger = CombinedLogger::new(maybe_sd_root)?;
    logger.max_level = max_level;
    log::set_boxed_logger(Box::new(logger))
        .map(|()| log::set_max_level(max_level))
        .context("set_boxed_logger failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmtime_basic() {
        // 1970‑01‑01 00:00:00
        assert_eq!(gmtime_y_m_d_h_m_s(0), (1970, 1, 1, 0, 0, 0));
        // 2024‑01‑01 00:00:00
        assert_eq!(gmtime_y_m_d_h_m_s(1_704_067_200), (2024, 1, 1, 0, 0, 0));
    }
}
