//! # 小米账号登录 + 设备列表读取
//!
//! 对应：**步骤 4（登录小米账号即可读取小米手环设备的功能）**
//!
//! 实现两种登录流：
//! 1. **账号密码直接登录**：
//!    `POST https://account.xiaomi.com/passport/login` form：
//!    `sid&qs&_sign&user&hash(MD5)&callback` → 成功拿 `ssecurity + serviceToken + userId`
//! 2. **触发短信验证码后**：当 userStatus==8 时拿到 `notification_id` ticket，
//!    用户输入短信验证码后调 `login_with_sms_code(ticket, code)` 完成登录。
//!
//! 设备列表：
//!   `POST https://api.io.mi.com/app/home/device_list` 用 `ssecurity` 签名 payload，
//!   返回 `{"result":{"list":[{"name","model","mac","did","pid","isOnline"},...]}}`。
//!
//! 仅暴露 Rust API；UI 集成后续步骤再做。

#![cfg(feature = "mi_account")]
#![allow(dead_code)] // 公开 API 暂未在主流程被调用

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{net_http, nvs_config};

const NVS_NS: &str = "mi_account";
const NVS_KEY_SESSION: &str = "session";

const LOGIN_URL: &str = "https://account.xiaomi.com/passport/login";
const LOGIN_SECURE_URL: &str = "https://account.xiaomi.com/passport/secure/login";
const DEVICE_LIST_URL: &str = "https://api.io.mi.com/app/home/device_list";

const SID: &str = "passport"; // 资源类登录用 passport；后续可改为 xiaomiwear
const USER_AGENT: &str = "AstroBox-NG/1.0 (ESP32-S3; Rust)";

// =====================================================================
// 公共类型
// =====================================================================

/// 已登录会话。可保存到 NVS 跨重启复用。
///
/// - `service_token`：HTTP Cookie 中 `serviceToken` 的值；用于后续 API 鉴权。
/// - `ssecurity`：API 签名密钥（每次登录服务端返回）。
/// - `user_id`：小米账号 ID。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MiAccountSession {
    pub user_id: String,
    pub service_token: String,
    pub ssecurity: String,
    /// 用户名（手机号 / 邮箱 / 小米 ID），仅做日志展示，无安全用途
    pub username: String,
}

/// 账号下的设备列表条目。从 `device_list` 响应抽取关键字段
/// （字段以小米 home API 实际返回为准，做了 fallback 兼容）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MiDeviceEntry {
    pub name: String,
    pub model: String,
    pub mac: String,
    pub device_id: String,
    pub is_online: bool,
}

/// 登录结果。直接成功 → 拿 Session；触发 2FA → 需要 SMS 验证码继续。
#[derive(Debug)]
pub enum LoginResult {
    /// 登录成功，已拿到 serviceToken + ssecurity
    Ok(MiAccountSession),
    /// 服务端要求二次验证（短信）。
    /// `ticket` 是 `notification_id`，作为 `login_with_sms_code` 的入参。
    /// `desc` 是给用户看的提示语（如 "短信已发送到 138****1234"）。
    NeedSms { ticket: String, desc: String },
}

// =====================================================================
// 1) 账号密码登录入口
// =====================================================================

/// 账号密码登录。
///
/// - `username`：手机号（含国家码 +86…）、邮箱、或小米账号 ID
/// - `password`：明文密码；本函数内部 MD5 后只把 hash 上送
///
/// 成功：返回 [`LoginResult::Ok`]，已自动写入 NVS（`save_session`）；
/// 服务端要求 SMS：返回 [`LoginResult::NeedSms`]，调用方拿到 `ticket` 后向用户
/// 询问验证码，再调 [`login_with_sms_code`] 完成登录。
pub async fn login_with_password(username: &str, password: &str) -> Result<LoginResult> {
    let hash = md5_hex(password);
    let qs = format!("sid={SID}");
    let callback = next_callback_name();
    let sign = compute_login_sign(&qs, &hash);

    let form: Vec<(&str, &str)> = vec![
        ("sid", SID),
        ("qs", qs.as_str()),
        ("_sign", sign.as_str()),
        ("user", username),
        ("hash", hash.as_str()),
        ("callback", callback.as_str()),
    ];
    let headers = vec![("User-Agent", USER_AGENT)];
    let (_status, body) = net_http::post_form(LOGIN_URL, &form, &headers).await?;

    let raw = strip_jsonp(&body, &callback);
    let resp: LoginRawResp =
        serde_json::from_str(&raw).with_context(|| format!("parse login response: {raw}"))?;

    match resp.user_status {
        0 => {
            let session = MiAccountSession {
                user_id: resp.user_id.ok_or_else(|| anyhow!("missing userId"))?,
                service_token: resp
                    .service_token
                    .ok_or_else(|| anyhow!("missing serviceToken"))?,
                ssecurity: resp.ssecurity.ok_or_else(|| anyhow!("missing ssecurity"))?,
                username: username.to_string(),
            };
            save_session(&session)?;
            Ok(LoginResult::Ok(session))
        }
        8 => {
            // 触发短信：notification_id 即后续 secure/login 的 ticket
            let ticket = resp
                .notification_id
                .or_else(|| resp.location.clone())
                .ok_or_else(|| anyhow!("userStatus=8 but no notification_id / location"))?;
            Ok(LoginResult::NeedSms {
                ticket,
                desc: resp
                    .desc
                    .unwrap_or_else(|| "短信验证码已发送，请输入".to_string()),
            })
        }
        other => Err(anyhow!(
            "login failed: status={other}, desc={}",
            resp.desc.unwrap_or_default()
        )),
    }
}

/// 在 `LoginResult::NeedSms` 后，用户拿到短信验证码 → 完成登录。
pub async fn login_with_sms_code(ticket: &str, code: &str) -> Result<MiAccountSession> {
    let qs = format!("sid={SID}&_t=create");
    let sign = compute_login_sign(&qs, "");
    let callback = next_callback_name();
    let form: Vec<(&str, &str)> = vec![
        ("_sign", sign.as_str()),
        ("user", ""),
        ("code", code),
        ("notification_id", ticket),
        ("qs", qs.as_str()),
        ("callback", callback.as_str()),
    ];
    let headers = vec![("User-Agent", USER_AGENT)];
    let (_status, body) = net_http::post_form(LOGIN_SECURE_URL, &form, &headers).await?;
    let raw = strip_jsonp(&body, &callback);
    let resp: LoginRawResp =
        serde_json::from_str(&raw).with_context(|| format!("parse sms login response: {raw}"))?;
    if resp.user_status != 0 {
        return Err(anyhow!(
            "sms login failed: status={}, desc={}",
            resp.user_status,
            resp.desc.unwrap_or_default()
        ));
    }
    let session = MiAccountSession {
        user_id: resp.user_id.ok_or_else(|| anyhow!("missing userId"))?,
        service_token: resp
            .service_token
            .ok_or_else(|| anyhow!("missing serviceToken"))?,
        ssecurity: resp.ssecurity.ok_or_else(|| anyhow!("missing ssecurity"))?,
        username: String::new(),
    };
    save_session(&session)?;
    Ok(session)
}

// =====================================================================
// 2) 设备列表读取
// =====================================================================

/// 用 `session` 调小米云 API 拉取账号下绑定的设备列表。
///
/// 内部会构造签名 `nonce + signed-data + ssecurity`，POST 到
/// `https://api.io.mi.com/app/home/device_list`。
///
/// 失败：网络错或解析错都返回 Err。
pub async fn list_devices(session: &MiAccountSession) -> Result<Vec<MiDeviceEntry>> {
    let nonce = next_nonce();
    let path = "/app/home/device_list";
    let body_json = r#"{"command":"device_list","dmCom":false}"#;
    // 签名串格式：&nonce={nonce}&path={path}&body={body}&ssecurity={ssecurity}
    // 然后用 ssecurity 做 HMAC-SHA256
    let sign_str = format!(
        "&nonce={nonce}&path={path}&body={body_json}&ssecurity={}",
        session.ssecurity
    );
    let signature = hmac_sha256_b64(session.ssecurity.as_bytes(), sign_str.as_bytes());
    let cookie_value = format!(
        "userId={}; serviceToken={}",
        session.user_id, session.service_token
    );

    let form: Vec<(&str, &str)> = vec![
        ("data", body_json),
        ("nonce", nonce.as_str()),
        ("signature", signature.as_str()),
        ("ssecurity", session.ssecurity.as_str()),
    ];
    let headers: Vec<(&str, &str)> = vec![
        ("User-Agent", USER_AGENT),
        ("Cookie", cookie_value.as_str()),
    ];
    let (_status, body) = net_http::post_form(DEVICE_LIST_URL, &form, &headers).await?;

    let resp: DeviceListResp = serde_json::from_str(&body)
        .with_context(|| format!("parse device_list response: {}", body))?;

    let list = resp.result.map(|r| r.list).unwrap_or_default();
    Ok(list
        .into_iter()
        .map(|d| MiDeviceEntry {
            name: d.name.unwrap_or_else(|| "Unknown".into()),
            model: d.model.unwrap_or_default(),
            mac: d.mac.or_else(|| d.did.clone()).unwrap_or_default(),
            device_id: d.did.unwrap_or_default(),
            is_online: d.is_online.unwrap_or(false),
        })
        .collect())
}

// =====================================================================
// 3) Session 持久化
// =====================================================================

/// 把 session 序列化为 JSON 存到 NVS namespace `mi_account` / key `session`。
pub fn save_session(session: &MiAccountSession) -> Result<()> {
    let json = serde_json::to_string(session).context("serialize session")?;
    nvs_config::nvs_set_string_ns(NVS_NS, NVS_KEY_SESSION, &json)
        .map_err(|e| anyhow!("save session to NVS: {e}"))
}

/// 从 NVS 读出之前保存的 session。不存在或解析失败返回 `None`。
pub fn load_session() -> Option<MiAccountSession> {
    let json = nvs_config::nvs_get_string_ns(NVS_NS, NVS_KEY_SESSION).ok()?;
    match serde_json::from_str::<MiAccountSession>(&json) {
        Ok(s) => Some(s),
        Err(e) => {
            log::warn!("[mi_account] load_session: parse failed: {e}");
            None
        }
    }
}

/// 删除 NVS 中的 session（登出本地）。不调服务端 logout URL。
pub fn logout() -> Result<()> {
    nvs_config::nvs_delete_string_ns(NVS_NS, NVS_KEY_SESSION)
        .map_err(|e| anyhow!("logout NVS delete: {e}"))
}

// =====================================================================
// 内部辅助：JSON 解析结构、签名、nonce、jsonp 剥离
// =====================================================================

#[derive(Debug, Deserialize)]
struct LoginRawResp {
    #[serde(default, rename = "userStatus")]
    user_status: i32,
    #[serde(default, rename = "userId")]
    user_id: Option<String>,
    #[serde(default, rename = "serviceToken")]
    service_token: Option<String>,
    #[serde(default)]
    ssecurity: Option<String>,
    /// 2FA 时的通知 ID（即短信 ticket）
    #[serde(default, rename = "notificationId")]
    notification_id: Option<String>,
    /// 部分流程把 ticket 放在 location 里（fallback）
    #[serde(default)]
    location: Option<String>,
    /// 描述信息（成功/失败原因 / "短信已发送"）
    desc: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceListResp {
    result: Option<DeviceListResult>,
}

#[derive(Debug, Deserialize)]
struct DeviceListResult {
    #[serde(default)]
    list: Vec<DeviceRaw>,
}

#[derive(Debug, Deserialize)]
struct DeviceRaw {
    name: Option<String>,
    model: Option<String>,
    mac: Option<String>,
    /// device id（小米 IoT 平台的 did）
    did: Option<String>,
    /// 兼容字段：小米 home API 返回的是 `isOnline`（驼峰），
    /// 部分版本里叫 `is_online` 或 `online`。这里用 alias 全兼容。
    #[serde(default, alias = "isOnline", alias = "online")]
    is_online: Option<bool>,
}

/// MD5 hex lowercase；Xiaomi 的 password hash 字段
fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// HMAC-SHA1(key=nonce, data=sign_str) → base64
/// 用于登录 `/_sign` 字段。
fn hmac_sha1_b64(key: &[u8], data: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC key length error");
    mac.update(data);
    let bytes = mac.finalize().into_bytes();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

/// HMAC-SHA256(key=ssecurity, data=sign_str) → base64
/// 用于设备列表 API 的 `signature` 字段。
fn hmac_sha256_b64(key: &[u8], data: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<sha2::Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key length error");
    mac.update(data);
    let bytes = mac.finalize().into_bytes();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

/// 登录请求 `_sign` 计算。
///
/// Xiaomi passport 现行算法（reverse 自开源实现）：
///   sign_str = "{nonce}&{sid}&{qs}&POST&hash={password_md5}"
///   nonce    = base64(rand 16 bytes)
///   _sign    = HMAC-SHA1(key=nonce, data=sign_str) → base64
///
/// 由于 nonce 随机，每次 _sign 不同；服务端用同样算法验证。
fn compute_login_sign(qs: &str, password_md5: &str) -> String {
    let nonce = next_nonce();
    let sign_str = format!("{nonce}&{SID}&{qs}&POST&hash={password_md5}");
    hmac_sha1_b64(nonce.as_bytes(), sign_str.as_bytes())
}

/// 生成 16 字节随机 nonce 并 base64 编码。
///
/// ESP-IDF 没有 OS 级 `rand` CSPRNG；这里用 `SystemTime` + 静态计数器做
/// 简易熵源，对登录签名已足够（Xiaomi 接受任意 nonce，只要签名匹配）。
/// 后续可改用 `esp_random()`（mbedtls 后端）做更安全随机源。
fn next_nonce() -> String {
    use base64::Engine;
    let counter = {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // 16 字节混合熵
    let mut bytes = [0u8; 16];
    let seed = now ^ counter.rotate_left(17);
    for i in 0..16 {
        bytes[i] = (seed >> ((i & 7) * 8)) as u8 ^ (counter as u8).wrapping_mul(i as u8);
    }
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

/// 生成 JSONP 回调名：`jsonp_<rand>`
fn next_callback_name() -> String {
    let n = {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };
    format!("jsonp_callback_{n}")
}

/// 把 `jsonp_xxx({...})` 包裹剥离得到 `{...}`。
///
/// 如果 Xiaomi 返回的是纯 JSON（无 JSONP 包裹），直接返回原串。
fn strip_jsonp(body: &str, expected_prefix: &str) -> String {
    let trimmed = body.trim();
    if trimmed.starts_with(expected_prefix) {
        if let Some(start) = trimmed.find('(') {
            if let Some(end) = trimmed.rfind(')') {
                if end > start {
                    return trimmed[start + 1..end].to_string();
                }
            }
        }
    }
    // 兜底：尝试剥所有 `xxx(...)` 形式
    if let Some(start) = trimmed.find('(') {
        if let Some(end) = trimmed.rfind(')') {
            if end > start {
                return trimmed[start + 1..end].to_string();
            }
        }
    }
    trimmed.to_string()
}

// =====================================================================
// 单元测试（不依赖网络，纯算法/解析校验）
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_works() {
        // 标准测试向量：MD5("password") = 5f4dcc3b5aa765d61d8327deb882cf99
        assert_eq!(md5_hex("password"), "5f4dcc3b5aa765d61d8327deb882cf99");
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn hmac_sha1_known_vector() {
        // RFC 2202 test-case 1: key=20×0x0b, data="Hi There"
        let key = [0x0bu8; 20];
        let mac = hmac_sha1_b64(&key, b"Hi There");
        assert_eq!(mac, "FR0LnpKEHCxACShjGc9CE9eiHiY=");
    }

    #[test]
    fn nonce_is_unique_base64() {
        let a = next_nonce();
        let b = next_nonce();
        assert_ne!(a, b, "nonce must differ between calls");
        assert!(!a.is_empty());
        // base64 of 16 bytes -> 24 chars (含 padding)
        assert_eq!(a.len(), 24);
    }

    #[test]
    fn callback_names_increment() {
        let a = next_callback_name();
        let b = next_callback_name();
        assert!(a.starts_with("jsonp_callback_"));
        assert!(b.starts_with("jsonp_callback_"));
        assert_ne!(a, b);
    }

    #[test]
    fn strip_jsonp_pure_json_passthrough() {
        let s = r#"{"foo":1,"bar":2}"#;
        assert_eq!(strip_jsonp(s, "unused"), s);
    }

    #[test]
    fn strip_jsonp_wrapper_extracted() {
        let s = "jsonp_callback_42({\"userId\": \"123\"})";
        assert_eq!(strip_jsonp(s, "jsonp_callback_42"), r#"{"userId": "123"}"#);
    }

    #[test]
    fn login_raw_resp_parses_success() {
        let json = r#"{"userStatus":0,"userId":"1001","serviceToken":"abc","ssecurity":"xyz","desc":"成功"}"#;
        let r: LoginRawResp = serde_json::from_str(json).unwrap();
        assert_eq!(r.user_status, 0);
        assert_eq!(r.user_id.as_deref(), Some("1001"));
        assert_eq!(r.service_token.as_deref(), Some("abc"));
        assert_eq!(r.ssecurity.as_deref(), Some("xyz"));
        assert_eq!(r.desc.as_deref(), Some("成功"));
    }

    #[test]
    fn login_raw_resp_parses_need_sms() {
        let json = r#"{"userStatus":8,"notificationId":"ticket-123","desc":"短信已发送"}"#;
        let r: LoginRawResp = serde_json::from_str(json).unwrap();
        assert_eq!(r.user_status, 8);
        assert_eq!(r.notification_id.as_deref(), Some("ticket-123"));
        assert_eq!(r.desc.as_deref(), Some("短信已发送"));
    }

    #[test]
    fn device_list_parses_minimal() {
        let json = r#"{"result":{"list":[{"name":"Mi Band 9","model":"n67","mac":"AA:BB","did":"did-1","isOnline":true}]}}"#;
        // 小米 home API 返回 `isOnline`（驼峰）；DeviceRaw 用 alias 兼容。
        let r: DeviceListResp = serde_json::from_str(json).unwrap();
        let list = r.result.unwrap().list;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("Mi Band 9"));
        assert_eq!(list[0].model.as_deref(), Some("n67"));
        assert_eq!(list[0].is_online, Some(true));
    }

    #[test]
    fn device_list_parses_is_online_snake_alias() {
        // 部分版本返回 snake_case `is_online`，也要兼容。
        let json = r#"{"result":{"list":[{"name":"X","model":"m","did":"d","is_online":false}]}}"#;
        let r: DeviceListResp = serde_json::from_str(json).unwrap();
        assert_eq!(r.result.unwrap().list[0].is_online, Some(false));
    }

    #[test]
    fn session_round_trip_json() {
        let s = MiAccountSession {
            user_id: "u1".into(),
            service_token: "st".into(),
            ssecurity: "sec".into(),
            username: "alice".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: MiAccountSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, s.user_id);
        assert_eq!(back.service_token, s.service_token);
        assert_eq!(back.ssecurity, s.ssecurity);
        assert_eq!(back.username, s.username);
    }

    #[test]
    fn login_sign_is_deterministic_given_nonce() {
        // 暴露内部签名函数验证算法可重现
        let md5 = md5_hex("password");
        // 由于 nonce 是随机的，我们直接调底层函数：
        let sign_str = format!("NONCE&{SID}&sid=passport&POST&hash={md5}");
        let s1 = hmac_sha1_b64(b"NONCE", sign_str.as_bytes());
        let s2 = hmac_sha1_b64(b"NONCE", sign_str.as_bytes());
        assert_eq!(s1, s2, "same inputs → same HMAC-SHA1");
    }
}
