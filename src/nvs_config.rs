use esp_idf_svc::sys::{
    nvs_close, nvs_commit, nvs_flash_init, nvs_get_str, nvs_open, nvs_set_str,
    ESP_ERR_NVS_NOT_FOUND, ESP_OK,
};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

const NVS_NAMESPACE: &str = "wifi_cfg";
const SSID_KEY: &str = "ssid";
const PASSWORD_KEY: &str = "password";
const NVS_READWRITE: u32 = 0x00000002;
const NVS_READONLY: u32 = 0x00000001;

/// 通用 namespace 下的最大字符串长度。小米账号 session JSON 可能 > 65B，
/// 因此独立常量放宽到 2 KB。
const MAX_STR_SIZE_GENERIC: usize = 2048;

static NVS_INIT: Once = Once::new();
static NVS_INIT_OK: AtomicBool = AtomicBool::new(false);

pub fn ensure_nvs_initialized() -> bool {
    NVS_INIT.call_once(|| {
        // SAFETY: nvs_flash_init() is called exactly once via Once::call_once.
        // This is the correct initialization pattern for ESP-IDF NVS in a
        // single-process embedded environment.
        let ret = unsafe { nvs_flash_init() };
        if ret == ESP_OK || ret == ESP_ERR_NVS_NOT_FOUND {
            NVS_INIT_OK.store(true, Ordering::Release);
        } else {
            log::error!("nvs_flash_init failed: {ret}");
        }
    });
    NVS_INIT_OK.load(Ordering::Acquire)
}

pub fn load_wifi_credentials() -> (String, String) {
    let default_ssid = option_env!("DEFAULT_WIFI_SSID").unwrap_or("").to_string();
    let default_password = option_env!("DEFAULT_WIFI_PASSWORD").unwrap_or("").to_string();

    if let Ok(ssid) = nvs_get_string(SSID_KEY) {
        if !ssid.is_empty() {
            let password = nvs_get_string(PASSWORD_KEY).unwrap_or(default_password.clone());
            return (ssid, password);
        }
    }

    if default_ssid.is_empty() {
        log::warn!(
            "No Wi-Fi credentials configured. Set DEFAULT_WIFI_SSID/DEFAULT_WIFI_PASSWORD \
             at compile time or configure via NVS at runtime."
        );
    }

    (default_ssid, default_password)
}

pub fn save_wifi_credentials(ssid: &str, password: &str) -> Result<(), String> {
    nvs_set_string(SSID_KEY, ssid)?;
    nvs_set_string(PASSWORD_KEY, password)?;
    Ok(())
}

// =====================================================================
// 通用 namespace 字符串存取（步骤 4：mi_account session JSON 持久化）
// =====================================================================

/// 在指定 namespace 下读字符串。不存在或读失败返回 Err。
/// 用于步骤 4 把 `MiAccountSession` 序列化为 JSON 后存到独立 namespace
/// (`mi_account`)，避免和 Wi-Fi 凭据混在 `wifi_cfg` 里。
///
/// 内部缓冲区使用 `MAX_STR_SIZE_GENERIC` (2 KB)，足够装 session JSON。
pub fn nvs_get_string_ns(namespace: &str, key: &str) -> Result<String, String> {
    if !ensure_nvs_initialized() {
        return Err("NVS not initialized".to_string());
    }

    let c_key = CString::new(key).map_err(|e| e.to_string())?;
    let c_ns = CString::new(namespace).map_err(|e| e.to_string())?;

    let mut handle: esp_idf_svc::sys::nvs_handle_t = 0;
    // SAFETY: nvs_open returns a handle that must be closed after use.
    let ret = unsafe { nvs_open(c_ns.as_ptr(), NVS_READONLY, &mut handle) };
    if ret != ESP_OK {
        return Err(format!("nvs_open failed: {ret}"));
    }

    let mut buf = vec![0u8; MAX_STR_SIZE_GENERIC];
    let mut len = buf.len() as usize;
    // SAFETY: buf 是有效可变 slice，len 初始化为 buf.len()。
    let ret = unsafe { nvs_get_str(handle, c_key.as_ptr(), buf.as_mut_ptr(), &mut len) };

    // SAFETY: handle was opened above and must be closed.
    unsafe { nvs_close(handle) };

    if ret != ESP_OK {
        return Err(format!("nvs_get_str failed: {ret}"));
    }

    let slice = &buf[..len as usize];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8(slice[..end].to_vec()).map_err(|e| format!("invalid UTF-8: {e}"))
}

/// 在指定 namespace 下写字符串。namespace 不存在会自动创建。
pub fn nvs_set_string_ns(namespace: &str, key: &str, value: &str) -> Result<(), String> {
    if !ensure_nvs_initialized() {
        return Err("NVS not initialized".to_string());
    }

    let c_key = CString::new(key).map_err(|e| e.to_string())?;
    let c_value = CString::new(value).map_err(|e| e.to_string())?;
    let c_ns = CString::new(namespace).map_err(|e| e.to_string())?;

    let mut handle: esp_idf_svc::sys::nvs_handle_t = 0;
    // SAFETY: nvs_open returns a handle that must be closed after use.
    let ret = unsafe { nvs_open(c_ns.as_ptr(), NVS_READWRITE, &mut handle) };
    if ret != ESP_OK {
        return Err(format!("nvs_open failed: {ret}"));
    }

    // SAFETY: handle is valid from the nvs_open call above.
    let ret = unsafe { nvs_set_str(handle, c_key.as_ptr(), c_value.as_ptr()) };
    if ret != ESP_OK {
        // SAFETY: handle was opened, must be closed on error path too.
        unsafe { nvs_close(handle) };
        return Err(format!("nvs_set_str failed: {ret}"));
    }

    // SAFETY: handle is valid, committing a write transaction.
    let ret = unsafe { nvs_commit(handle) };
    if ret != ESP_OK {
        // SAFETY: handle was opened, must be closed on error path too.
        unsafe { nvs_close(handle) };
        return Err(format!("nvs_commit failed: {ret}"));
    }

    // SAFETY: handle was opened above, closing it to release resources.
    unsafe { nvs_close(handle) };
    Ok(())
}

/// 删除指定 namespace 下的 key。不存在不算错误（返回 Ok）。
pub fn nvs_delete_string_ns(namespace: &str, key: &str) -> Result<(), String> {
    if !ensure_nvs_initialized() {
        return Err("NVS not initialized".to_string());
    }
    let c_key = CString::new(key).map_err(|e| e.to_string())?;
    let c_ns = CString::new(namespace).map_err(|e| e.to_string())?;

    let mut handle: esp_idf_svc::sys::nvs_handle_t = 0;
    let ret = unsafe { nvs_open(c_ns.as_ptr(), NVS_READWRITE, &mut handle) };
    if ret != ESP_OK {
        return Err(format!("nvs_open failed: {ret}"));
    }

    // SAFETY: 删除不存在 key 时 esp-idf 返回 ESP_ERR_NVS_NOT_FOUND，按 OK 处理。
    let ret = unsafe { esp_idf_svc::sys::nvs_erase_key(handle, c_key.as_ptr()) };
    if ret != ESP_OK && ret != ESP_ERR_NVS_NOT_FOUND {
        unsafe { nvs_close(handle) };
        return Err(format!("nvs_erase_key failed: {ret}"));
    }
    let ret = unsafe { nvs_commit(handle) };
    unsafe { nvs_close(handle) };
    if ret != ESP_OK {
        return Err(format!("nvs_commit failed: {ret}"));
    }
    Ok(())
}

fn nvs_get_string(key: &str) -> Result<String, String> {
    nvs_get_string_ns(NVS_NAMESPACE, key)
}

fn nvs_set_string(key: &str, value: &str) -> Result<(), String> {
    nvs_set_string_ns(NVS_NAMESPACE, key, value)
}
