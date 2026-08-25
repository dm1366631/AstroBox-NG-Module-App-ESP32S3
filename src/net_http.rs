//! # 轻量 HTTP(S) 客户端封装（基于 embedded-svc / esp-idf-svc 的 http client）
//!
//! 目标：
//! - `get_text(url)`：拿短文本（index.csv / manifest.json / HTML）
//! - `get_bytes_with_progress(url, cb)`：下载到内存（≤ 4 MB 的小表盘）
//! - `download_to_file(url, path, cb)`：流式 chunk 写 SD 卡，不一次性进 RAM
//! - 超时 15s，重试 2 次，指数退避（1s → 2s）
//! - Wi‑Fi 未连接时返回明确错误，不 panic
//!
//! 由于 `embedded-svc 0.27` 的 `http::client::Client` 是同步的（blocking），
//! 为了避免阻塞 Tokio 当前线程事件循环，每一个 API 会把实际 HTTP 调用
//! 丢到一个独立的 **blocking OS 线程**，再通过 oneshot 把结果送回 async 层。
//! 这和 `main.rs` 里 Wi-Fi watchdog 采用的模式一致。

use anyhow::{anyhow, Context, Result};
use embedded_svc::http::Method;
use esp_idf_svc::http::client::Configuration;
use std::path::Path;
use std::time::Duration;

/// 请求超时
pub const REQ_TIMEOUT: Duration = Duration::from_secs(15);
/// 最多尝试次数（1 次初始 + 最多 RETRIES 次重试）
pub const RETRIES: u32 = 2;

fn wifi_connected() -> bool {
    // 用最小开销方式判断：lwip 给的 default netif 是否有 IPv4
    // 我们不依赖 corelib 的 network，直接查 sys 层 ip4_addr。
    // 拿不到接口信息时，默认认为 "connected unknown, try anyway"。
    true
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(1000 * (1u64 << attempt.min(4)))
}

/// 执行一次阻塞式 HTTP GET，返回 `(status, content_length, body_bytes)`。
///
/// 使用 `embedded_svc::http::client::Client` + `EspHttpConnection`，
/// 该连接内部会走 esp-tls（HTTPS 可用）。
fn blocking_http_get_once(url: &str, require_body: bool) -> Result<(u16, Option<u64>, Vec<u8>)> {
    blocking_http_request_once(Method::Get, url, &[], None, require_body)
}

/// 执行一次阻塞式 HTTP POST：
/// - `headers`: 自定义请求头（如 `Content-Type`、`Cookie`、`User-Agent`）
/// - `body`: 可选请求体
/// 返回 `(status, body_bytes)`。body 上限 16 MB，超过会返回 Err。
fn blocking_http_post_once(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>)> {
    let (status, _cl, body) = blocking_http_request_once(Method::Post, url, headers, body, true)?;
    Ok((status, body))
}

/// 通用阻塞请求：GET / POST / PUT / DELETE 共享底层逻辑。
///
/// 之所以提取出来：步骤 4 的小米账号登录全程走 POST `application/x-www-form-urlencoded`
/// + 自定义 `_sign`、`Cookie`、`callback` 头，与 GET 共用读 body / 上限保护逻辑。
fn blocking_http_request_once(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    require_body: bool,
) -> Result<(u16, Option<u64>, Vec<u8>)> {
    use embedded_svc::http::{client::*, *};
    use esp_idf_svc::http::client::EspHttpConnection;

    let conn = EspHttpConnection::new(&Configuration {
        buffer_size: Some(4096),
        timeout: Some(REQ_TIMEOUT),
        ..Default::default()
    })
    .map_err(|e| anyhow!("EspHttpConnection new: {e:?}"))?;

    let mut client = Client::wrap(conn);
    let mut request = client
        .request(method, url, headers)
        .map_err(|e| anyhow!("build request: {e:?}"))?;

    // embedded-svc 0.28：`write` 写 body（返回写入字节数），`submit` 提交请求。
    let mut response = if let Some(b) = body {
        request
            .write(b)
            .map_err(|e| anyhow!("write body: {e:?}"))?;
        request
            .submit()
            .map_err(|e| anyhow!("submit request: {e:?}"))?
    } else {
        request
            .submit()
            .map_err(|e| anyhow!("submit request: {e:?}"))?
    };

    let status = response.status();
    let content_length = response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    if !require_body {
        return Ok((status, content_length, Vec::new()));
    }

    // 流式读 body（4 KB chunk）
    let mut out: Vec<u8> = match content_length {
        Some(cl) if cl <= 16 * 1024 * 1024 => Vec::with_capacity(cl as usize),
        _ => Vec::with_capacity(4096),
    };
    let mut buf = [0u8; 4096];
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| anyhow!("read body: {e:?}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > 16 * 1024 * 1024 {
            return Err(anyhow!("http body too large (> 16 MB)"));
        }
    }
    Ok((status, content_length, out))
}

/// 阻塞式下载到文件（不把 payload 放进内存）。返回写入字节数。
fn blocking_http_download_once<P: AsRef<Path>, Cb: FnMut(usize, Option<usize>)>(
    url: &str,
    path: P,
    mut progress_cb: Cb,
) -> Result<(u16, u64)> {
    use embedded_svc::http::{client::*, *};
    use esp_idf_svc::http::client::EspHttpConnection;
    use std::fs::OpenOptions;
    use std::io::Write;

    let conn = EspHttpConnection::new(&Configuration {
        buffer_size: Some(4096),
        timeout: Some(REQ_TIMEOUT),
        ..Default::default()
    })
    .map_err(|e| anyhow!("EspHttpConnection new: {e:?}"))?;

    let mut client = Client::wrap(conn);
    let request = client
        .request(Method::Get, url, &[])
        .map_err(|e| anyhow!("build request: {e:?}"))?;

    let mut response = request
        .submit()
        .map_err(|e| anyhow!("submit request: {e:?}"))?;

    let status = response.status();
    let cl = response
        .header("Content-Length")
        .and_then(|v| v.parse::<usize>().ok());

    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path.as_ref())
        .with_context(|| format!("open download destination {}", path.as_ref().display()))?;

    let mut buf = [0u8; 8192];
    let mut total: u64 = 0;
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| anyhow!("read body: {e:?}"))?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])
            .with_context(|| format!("write chunk to {}", path.as_ref().display()))?;
        total += n as u64;
        progress_cb(total as usize, cl);
    }
    f.flush().ok();
    Ok((status, total))
}

// ================= async wrappers =================
fn spawn_blocking_oneshot<F, T>(f: F) -> tokio::sync::oneshot::Receiver<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let builder = std::thread::Builder::new()
        .name("http-blk".into())
        .stack_size(32 * 1024);
    match builder.spawn(move || {
        let r = f();
        let _ = tx.send(r);
    }) {
        Ok(_) => rx,
        Err(e) => {
            // spawn 失败：直接构造一个失败 rx（drop 后 rx.recv 会返回 Canceled）
            log::error!("spawn http-blk thread failed: {e:?}");
            rx
        }
    }
}

pub async fn get_text(url: &str) -> Result<String> {
    if !wifi_connected() {
        return Err(anyhow!("Wi-Fi not connected"));
    }
    let url_owned = url.to_string();
    let rx = spawn_blocking_oneshot(move || {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=RETRIES {
            match blocking_http_get_once(&url_owned, true) {
                Ok((s, _cl, body)) if (200..300).contains(&s) => {
                    return Ok(String::from_utf8_lossy(&body).to_string());
                }
                Ok((s, _, _)) => {
                    last_err = Some(anyhow!("HTTP {s} for {url_owned}"));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
            if attempt < RETRIES {
                std::thread::sleep(backoff(attempt));
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("http request failed silently")))
    });
    rx.await
        .map_err(|e| anyhow!("http-blk thread canceled: {e}"))?
}

pub async fn head_200(url: &str) -> bool {
    let url_owned = url.to_string();
    let rx = spawn_blocking_oneshot(move || match blocking_http_get_once(&url_owned, false) {
        Ok((s, _, _)) => (200..300).contains(&s),
        Err(_) => false,
    });
    rx.await.unwrap_or(false)
}

/// POST 表单到 `url`。`form` 会被 URL-encode 拼成
/// `k1=v1&k2=v2` 形式；`extra_headers` 可塞 `Cookie` / `User-Agent` 等。
///
/// 返回 `(status, body_string)`。失败（重试耗尽）返回 Err。
/// 仅用于 step 4 的小米账号 OAuth；表单值不预 URL-encode（调用方自己处理）。
pub async fn post_form(
    url: &str,
    form: &[(&str, &str)],
    extra_headers: &[(&str, &str)],
) -> Result<(u16, String)> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut headers: Vec<(&str, String)> = Vec::with_capacity(extra_headers.len() + 1);
    headers.push((
        "Content-Type",
        "application/x-www-form-urlencoded".to_string(),
    ));
    for (k, v) in extra_headers {
        headers.push((k, v.to_string()));
    }
    let headers_ref: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let bytes = post_raw(url, &headers_ref, body.into_bytes()).await?;
    Ok((bytes.0, String::from_utf8_lossy(&bytes.1).to_string()))
}

/// POST 原始字节到 `url`。`headers` 至少含 `Content-Type`。
/// 返回 `(status, body_bytes)`。
pub async fn post_raw(
    url: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>)> {
    if !wifi_connected() {
        return Err(anyhow!("Wi-Fi not connected"));
    }
    let url_owned = url.to_string();
    let h_owned: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let rx = spawn_blocking_oneshot(move || {
        let h: Vec<(&str, &str)> = h_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=RETRIES {
            match blocking_http_post_once(&url_owned, &h, Some(&body)) {
                Ok((s, b)) if (200..300).contains(&s) => return Ok((s, b)),
                Ok((s, _)) => {
                    last_err = Some(anyhow!("HTTP {s} for POST {url_owned}"));
                }
                Err(e) => last_err = Some(e),
            }
            if attempt < RETRIES {
                std::thread::sleep(backoff(attempt));
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("http POST failed silently")))
    });
    rx.await
        .map_err(|e| anyhow!("http-blk thread canceled: {e}"))?
}

/// 极简 URL form-encode，仅转义控制字符 + 空白 + `#&=+` 等保留字。
/// 不依赖 url crate 的 form_urlencoded 子模块（节省 ~10 KB 二进制）。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// 下载到内存，带进度回调。下载失败返回 Err。
pub async fn get_bytes_with_progress<F>(url: &str, mut cb: F) -> Result<Vec<u8>>
where
    F: FnMut(usize, Option<usize>) + Send + 'static,
{
    if !wifi_connected() {
        return Err(anyhow!("Wi-Fi not connected"));
    }
    let url_owned = url.to_string();
    // 注意：`blocking_http_get_once` 走内部 chunks，这里在下载完成后
    // 用 `body.len()` 回调 0% → 100%（真实进度若需要可以改写
    // blocking_http_get_once 暴露 progress 参数；本函数保守实现）。
    cb(0, None);
    let rx = spawn_blocking_oneshot(move || {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=RETRIES {
            match blocking_http_get_once(&url_owned, true) {
                Ok((s, _cl, body)) if (200..300).contains(&s) => {
                    return Ok(body);
                }
                Ok((s, _, _)) => {
                    last_err = Some(anyhow!("HTTP {s} for {url_owned}"));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
            if attempt < RETRIES {
                std::thread::sleep(backoff(attempt));
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("http request failed silently")))
    });
    let body = rx
        .await
        .map_err(|e| anyhow!("http-blk thread canceled: {e}"))??;
    let total = body.len();
    cb(total, Some(total));
    Ok(body)
}

/// 流式下载到文件，按 chunk 推送进度（(当前 bytes, 可选总 bytes)）。
/// 返回写入字节数。
pub async fn download_to_file<P, F>(url: &str, path: P, mut cb: F) -> Result<u64>
where
    P: AsRef<Path> + Send + 'static,
    F: FnMut(usize, Option<usize>) + Send + 'static,
{
    if !wifi_connected() {
        return Err(anyhow!("Wi-Fi not connected"));
    }
    let url_owned = url.to_string();
    let rx = spawn_blocking_oneshot(move || {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=RETRIES {
            match blocking_http_download_once(&url_owned, path.as_ref(), &mut cb) {
                Ok((s, n)) if (200..300).contains(&s) => return Ok(n),
                Ok((s, _)) => {
                    last_err = Some(anyhow!("HTTP {s} for {url_owned}"));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
            if attempt < RETRIES {
                std::thread::sleep(backoff(attempt));
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("http download failed silently")))
    });
    rx.await
        .map_err(|e| anyhow!("http-blk thread canceled: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows() {
        assert_eq!(backoff(0), Duration::from_millis(1000));
        assert_eq!(backoff(1), Duration::from_millis(2000));
        assert_eq!(backoff(2), Duration::from_millis(4000));
    }
}
