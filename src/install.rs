use corelib::{
    device::xiaomi::{
        components::{
            install::{InstallComponent, InstallSystem},
            resource::{ResourceComponent, ResourceSystem},
            thirdparty_app::{AppInfo, ThirdpartyAppComponent, ThirdpartyAppSystem},
            watchface::{WatchfaceComponent, WatchfaceSystem},
        },
        packet::mass::MassDataType,
    },
    ecs,
};
use log::info;
use std::path::{Path, PathBuf};
use tokio::{fs, sync::mpsc};

#[cfg(feature = "repo_net")]
use crate::{
    net_http,
    repo::{RepoItem, RepoManifest, RepoType},
    transfer::{TransferDirection, TransferProgress},
};

/// #19: Generic helper that encapsulates the recurring
/// `ecs::with_rt_mut + oneshot + rt.with_device_mut` access pattern.
///
/// Every `install_* / uninstall_* / list_*` function in this module used to
/// inline ~15 lines of identical `ecs::with_rt_mut(...) + oneshot + rx.await`
/// boilerplate; routing them through `with_device_async` drops the
/// duplication and centralises error surface.
pub(crate) async fn with_device_async<F, T>(addr: &str, f: F) -> anyhow::Result<T>
where
    F: FnOnce(&mut corelib::ecs::World, corelib::ecs::Entity) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let addr_owned = addr.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();

    ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_owned, |world, entity| {
            let result = f(world, entity);
            let _ = tx.send(result);
        });
    })
    .await;

    rx.await?
}

pub(crate) async fn resolve_app_info(addr: &str, package_name: &str) -> anyhow::Result<AppInfo> {
    let pkg_owned = package_name.to_string();
    with_device_async(addr, move |world, entity| {
        let component = world
            .get::<ResourceComponent>(entity)
            .ok_or_else(|| anyhow::anyhow!("ResourceComponent missing on device"))?;
        component
            .quick_apps
            .iter()
            .find(|item| item.package_name == pkg_owned)
            .map(|item| AppInfo {
                package_name: item.package_name.clone(),
                fingerprint: item.fingerprint.clone(),
            })
            .ok_or_else(|| anyhow::anyhow!("App {} not found in installed list", pkg_owned))
    })
    .await
}

pub async fn list_installed_watchfaces(addr: &str) -> anyhow::Result<Vec<String>> {
    let receiver = with_device_async(addr, |world, entity| {
        if world.get::<WatchfaceComponent>(entity).is_none() {
            anyhow::bail!("WatchfaceComponent missing");
        }
        if world.get::<ResourceComponent>(entity).is_none() {
            anyhow::bail!("ResourceComponent missing");
        }
        let mut system = world
            .get_mut::<ResourceSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("ResourceSystem missing"))?;
        Ok(system.request_watchface_list())
    })
    .await?;

    let watchfaces = receiver.await??;
    let names: Vec<String> = watchfaces
        .iter()
        .map(|w| format!("{} (id={})", w.name.clone(), w.id.clone()))
        .collect();
    info!("Device {} has {} watchface(s) installed", addr, names.len());
    Ok(names)
}

pub async fn list_installed_quick_apps(addr: &str) -> anyhow::Result<Vec<String>> {
    let receiver = with_device_async(addr, |world, entity| {
        if world.get::<ResourceComponent>(entity).is_none() {
            anyhow::bail!("ResourceComponent missing");
        }
        let mut system = world
            .get_mut::<ResourceSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("ResourceSystem missing"))?;
        Ok(system.request_quick_app_list())
    })
    .await?;

    let apps = receiver.await??;
    let names: Vec<String> = apps
        .iter()
        .map(|a| format!("{} (pkg={})", a.name.clone(), a.package_name.clone()))
        .collect();
    info!("Device {} has {} quick app(s) installed", addr, names.len());
    Ok(names)
}

pub async fn install_quick_app(
    addr: &str,
    package_name: &str,
    app_data: Vec<u8>,
) -> anyhow::Result<()> {
    info!(
        "Installing quick app {} ({}) on {}...",
        package_name,
        app_data.len(),
        addr
    );

    let pkg_owned = package_name.to_string();
    let future = with_device_async(addr, move |world, entity| {
        if world.get::<InstallComponent>(entity).is_none() {
            anyhow::bail!("InstallComponent missing");
        }
        let mut system = world
            .get_mut::<InstallSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("InstallSystem missing"))?;
        system
            .send_install_request(MassDataType::ThirdPartyApp, app_data, Some(&pkg_owned))
            .map_err(|e| anyhow::anyhow!("Failed to create install request: {e}"))
    })
    .await?;

    future.await.map_err(|e| anyhow::anyhow!(e.to_string()))?;

    info!(
        "Quick app {} installed successfully on {}",
        package_name, addr
    );
    Ok(())
}

pub async fn install_quick_app_from_file(
    addr: &str,
    package_name: &str,
    file_path: &str,
) -> anyhow::Result<()> {
    let data = fs::read(Path::new(file_path)).await?;
    install_quick_app(addr, package_name, data).await
}

pub async fn install_watchface(addr: &str, face_data: Vec<u8>) -> anyhow::Result<()> {
    info!("Installing watchface ({}) on {}...", face_data.len(), addr);

    let future = with_device_async(addr, move |world, entity| {
        if world.get::<InstallComponent>(entity).is_none() {
            anyhow::bail!("InstallComponent missing");
        }
        let mut system = world
            .get_mut::<InstallSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("InstallSystem missing"))?;
        system
            .send_install_request(MassDataType::Watchface, face_data, None)
            .map_err(|e| anyhow::anyhow!("Failed to create install request: {e}"))
    })
    .await?;

    future.await.map_err(|e| anyhow::anyhow!(e.to_string()))?;

    info!("Watchface installed successfully on {}", addr);
    Ok(())
}

pub async fn install_watchface_from_file(addr: &str, file_path: &str) -> anyhow::Result<()> {
    let data = fs::read(Path::new(file_path)).await?;
    install_watchface(addr, data).await
}

pub async fn uninstall_quick_app(addr: &str, package_name: &str) -> anyhow::Result<()> {
    info!("Uninstalling quick app {} from {}...", package_name, addr);

    let pkg_owned = package_name.to_string();
    with_device_async(addr, move |world, entity| {
        if world.get::<ThirdpartyAppComponent>(entity).is_none() {
            anyhow::bail!("ThirdpartyAppComponent missing");
        }
        let mut system = world
            .get_mut::<ThirdpartyAppSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("ThirdpartyAppSystem missing"))?;
        let app_info = AppInfo {
            package_name: pkg_owned.clone(),
            fingerprint: vec![],
        };
        system.uninstall_app(&app_info);
        info!("Uninstall request sent for {}", pkg_owned);
        Ok(())
    })
    .await
}

pub async fn uninstall_watchface(addr: &str, watchface_id: &str) -> anyhow::Result<()> {
    info!("Uninstalling watchface {} from {}...", watchface_id, addr);

    let id_owned = watchface_id.to_string();
    with_device_async(addr, move |world, entity| {
        if world.get::<WatchfaceComponent>(entity).is_none() {
            anyhow::bail!("WatchfaceComponent missing");
        }
        let mut system = world
            .get_mut::<WatchfaceSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("WatchfaceSystem missing"))?;
        system.uninstall_watchface(&id_owned);
        info!("Uninstall watchface request sent for {}", id_owned);
        Ok(())
    })
    .await
}

pub async fn set_watchface(addr: &str, watchface_id: &str) -> anyhow::Result<()> {
    info!("Setting watchface {} on {}...", watchface_id, addr);

    let id_owned = watchface_id.to_string();
    with_device_async(addr, move |world, entity| {
        if world.get::<WatchfaceComponent>(entity).is_none() {
            anyhow::bail!("WatchfaceComponent missing");
        }
        let mut system = world
            .get_mut::<WatchfaceSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("WatchfaceSystem missing"))?;
        system.set_watchface(&id_owned);
        info!("Set watchface request sent for {}", id_owned);
        Ok(())
    })
    .await
}

pub async fn launch_quick_app(addr: &str, package_name: &str) -> anyhow::Result<()> {
    info!("Launching quick app {} on {}...", package_name, addr);

    let app_info = resolve_app_info(addr, package_name).await?;

    with_device_async(addr, move |world, entity| {
        if world.get::<ThirdpartyAppComponent>(entity).is_none() {
            anyhow::bail!("ThirdpartyAppComponent missing");
        }
        let mut system = world
            .get_mut::<ThirdpartyAppSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("ThirdpartyAppSystem missing"))?;
        system.launch_app(&app_info, "");
        info!("Launch request sent for {}", package_name);
        Ok(())
    })
    .await
}

pub async fn send_phone_message(
    addr: &str,
    package_name: &str,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    info!(
        "Sending phone message to app {} on {} ({} bytes)...",
        package_name,
        addr,
        payload.len()
    );

    let app_info = resolve_app_info(addr, package_name).await?;

    with_device_async(addr, move |world, entity| {
        if world.get::<ThirdpartyAppComponent>(entity).is_none() {
            anyhow::bail!("ThirdpartyAppComponent missing");
        }
        let mut system = world
            .get_mut::<ThirdpartyAppSystem>(entity)
            .ok_or_else(|| anyhow::anyhow!("ThirdpartyAppSystem missing"))?;
        system.send_phone_message(&app_info, payload);
        info!("Phone message sent to app {}", package_name);
        Ok(())
    })
    .await
}

// =====================================================================
// 联网资源 → 安装流水线（feature repo_net）
// =====================================================================

/// 下载 + 安装；若 `cache_to_sd=true` 则同时落盘到
/// `/sdcard/astrobox/cache/<slug>_<version>.<ext>`，下次命中时跳过
/// 下载。
///
/// 进度：下载占 0%~50%，安装占 50%~100%。
#[cfg(feature = "repo_net")]
pub async fn install_from_repo(
    addr: &str,
    item: &RepoItem,
    manifest: &RepoManifest,
    cache_to_sd: bool,
    sd_root: Option<&Path>,
    progress_tx: Option<mpsc::Sender<TransferProgress>>,
) -> anyhow::Result<()> {
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicU32, Ordering};

    // 1) 过滤三重保障（paid 再检查一次）
    if !item.paid.is_free() {
        return Err(anyhow!(
            "install_from_repo: item {} is paid (合规过滤拒绝)",
            item.name
        ));
    }
    // 2) 选 payload URL
    let Some(url) = manifest.payload_url(item.restype) else {
        return Err(anyhow!(
            "manifest has no download url for {:?} (item={})",
            item.restype,
            item.name
        ));
    };

    // 3) 生成缓存路径（若可用）
    let cache_path: Option<PathBuf> = match (cache_to_sd, sd_root) {
        (true, Some(root)) => {
            let slug = slugify(&item.name);
            let version = manifest.version.clone().unwrap_or_else(|| "latest".into());
            let ext = match item.restype {
                RepoType::QuickApp => "rpk",
                RepoType::Watchface => "mwz",
            };
            Some(
                root.join("astrobox/cache")
                    .join(format!("{slug}_{version}.{ext}")),
            )
        }
        _ => None,
    };

    // 4) 命中缓存？(大小 ≥ manifest.filesize 阈值 99% 视为一致)
    let cache_hit = match (&cache_path, manifest.filesize) {
        (Some(p), Some(expected)) => match fs::metadata(p).await {
            Ok(m) => m.len() >= expected.saturating_mul(99) / 100,
            Err(_) => false,
        },
        (Some(p), None) => fs::metadata(p).await.is_ok_and(|m| m.len() > 0),
        _ => false,
    };

    let total = manifest.filesize.map(|n| n as usize);
    let file_name = item.name.clone();
    let emit = |pct, cur| {
        let tx = progress_tx.clone();
        async move {
            let Some(tx) = tx else { return };
            let _ = tx
                .send(TransferProgress {
                    direction: TransferDirection::Send,
                    progress_percent: pct,
                    current_bytes: cur,
                    total_bytes: total,
                    file_name: file_name.clone(),
                })
                .await;
        }
    };

    let local_bytes: Vec<u8> = if cache_hit {
        let p = cache_path.as_ref().unwrap();
        info!(
            "[Repo] cache hit: {}, install directly from SD",
            p.display()
        );
        emit(50.0, total.unwrap_or(0)).await;
        fs::read(p)
            .await
            .map_err(|e| anyhow!("read cached package {}: {e:#}", p.display()))?
    } else {
        // 下载
        let current = AtomicU32::new(0);
        let total_arc = std::sync::Arc::new(total);
        if let Some(p) = &cache_path {
            // 方式 A：流式写文件 + 后续读文件
            let _ = crate::sdcard::ensure_dir(p.parent().expect("cache in astrobox/cache"));
            let p_clone = p.clone();
            let total_for_cb = *total_arc;
            let downloaded = crate::net_http::download_to_file(url, p_clone.clone(), |cur, ttl| {
                let ttl = ttl.or(total_for_cb);
                let pct = 50.0
                    * (match ttl {
                        Some(0) | None => 0.0,
                        Some(max) => (cur as f32 / max as f32).clamp(0.0, 1.0),
                    });
                current.store(cur as u32, Ordering::Relaxed);
                let tx = progress_tx.clone();
                // 这里同步上下文，不 .await；直接 best-effort try_send
                if let Some(tx) = tx {
                    let _ = tx.try_send(TransferProgress {
                        direction: TransferDirection::Send,
                        progress_percent: pct,
                        current_bytes: cur,
                        total_bytes: ttl,
                        file_name: file_name.clone(),
                    });
                }
            })
            .await?;
            emit(50.0, downloaded as usize).await;
            fs::read(p)
                .await
                .map_err(|e| anyhow!("read downloaded package {}: {e:#}", p.display()))?
        } else {
            // 方式 B：直接进内存
            let tx_for_cb = progress_tx.clone();
            let total_local = total;
            let bytes = net_http::get_bytes_with_progress(url, move |cur, ttl| {
                let ttl = ttl.or(total_local);
                let pct = 50.0
                    * (match ttl {
                        Some(0) | None => 0.0,
                        Some(max) => (cur as f32 / max as f32).clamp(0.0, 1.0),
                    });
                current.store(cur as u32, Ordering::Relaxed);
                if let Some(tx) = tx_for_cb.as_ref() {
                    let _ = tx.try_send(TransferProgress {
                        direction: TransferDirection::Send,
                        progress_percent: pct,
                        current_bytes: cur,
                        total_bytes: ttl,
                        file_name: file_name.clone(),
                    });
                }
            })
            .await?;
            emit(50.0, bytes.len()).await;
            bytes
        }
    };

    // 5) 安装（50% → 100%）
    emit(51.0, local_bytes.len()).await;
    match item.restype {
        RepoType::QuickApp => {
            let pkg = manifest
                .package_name
                .clone()
                .or_else(|| {
                    item.manifest_path
                        .rsplit('/')
                        .next()
                        .map(|s| s.trim_end_matches(".json").to_string())
                })
                .unwrap_or_else(|| slugify(&item.name));
            install_quick_app(addr, &pkg, local_bytes).await?;
        }
        RepoType::Watchface => {
            install_watchface(addr, local_bytes).await?;
        }
    }
    emit(100.0, total.unwrap_or(0)).await;
    Ok(())
}

#[cfg(feature = "repo_net")]
fn slugify(s: &str) -> String {
    // 非字母数字字符 → "_"，限制长度 64。
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out.truncate(64);
    if out.is_empty() {
        "item".into()
    } else {
        out
    }
}

#[cfg(feature = "repo_net")]
#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_works() {
        assert_eq!(slugify("倒数日 快应用 (v1.0)"), "____________v1_0_");
        assert_eq!(slugify("HyperBili"), "hyperbili");
        assert_eq!(slugify(""), "item");
    }
}
