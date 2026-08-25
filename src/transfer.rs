use corelib::{
    device::xiaomi::{
        components::{
            mass::{MassComponent, MassSystem, SendMassCallbackData},
            resource::ResourceComponent,
            thirdparty_app::{AppInfo, ThirdpartyAppComponent, ThirdpartyAppSystem},
        },
        packet::mass::MassDataType,
    },
    ecs, events,
};
use log::{error, info, warn};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub struct TransferProgress {
    pub direction: TransferDirection,
    pub progress_percent: f32,
    pub current_bytes: usize,
    pub total_bytes: Option<usize>,
    pub file_name: String,
}

#[derive(Clone, Debug)]
pub enum TransferDirection {
    Send,
    Receive,
}

impl std::fmt::Display for TransferDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferDirection::Send => write!(f, "TX →"),
            TransferDirection::Receive => write!(f, "RX ←"),
        }
    }
}

pub async fn send_data_to_device(
    addr: &str,
    data_type: MassDataType,
    data: Vec<u8>,
) -> anyhow::Result<()> {
    let addr_owned = addr.to_string();
    let (tx, rx) = oneshot::channel();

    // `with_device_mut` is a synchronous closure, so we can't await inside it.
    // Extract the owner_id synchronously, then run the async transfer outside.
    let tx_err = tx.clone();
    let owner_id: Option<String> = ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_owned, |world, entity| {
            if world.get::<MassComponent>(entity).is_none() {
                let _ = tx_err.send(Err(anyhow::anyhow!(
                    "MassComponent missing on device {}",
                    addr_owned
                )));
                return None;
            }
            match world.get::<MassSystem>(entity) {
                Some(s) => Some(s.owner_id.clone()),
                None => {
                    let _ = tx_err.send(Err(anyhow::anyhow!(
                        "MassSystem missing on device {}",
                        addr_owned
                    )));
                    None
                }
            }
        })
        .flatten()
    })
    .await;

    let owner_id = match owner_id {
        Some(id) => id,
        None => return rx.await??,
    };

    let result = corelib::device::xiaomi::components::mass::send_file_for_owner(
        owner_id,
        data,
        data_type,
        |_cb: SendMassCallbackData| {},
    )
    .await;
    let _ = tx.send(Ok(result.map(|_| ())));
    rx.await??;
    Ok(())
}

pub async fn send_data_to_device_with_progress<F>(
    addr: &str,
    data_type: MassDataType,
    data: Vec<u8>,
    progress_cb: F,
) -> anyhow::Result<()>
where
    F: Fn(SendMassCallbackData) + Send + Sync + 'static,
{
    let addr_owned = addr.to_string();
    let (tx, rx) = oneshot::channel();
    let cb_arc: Arc<dyn Fn(SendMassCallbackData) + Send + Sync> = Arc::new(progress_cb);

    // Synchronous closure: extract owner_id, run async transfer outside.
    let tx_err = tx.clone();
    let owner_id: Option<String> = ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_owned, |world, entity| {
            if world.get::<MassComponent>(entity).is_none() {
                let _ = tx_err.send(Err(anyhow::anyhow!(
                    "MassComponent missing on device {}",
                    addr_owned
                )));
                return None;
            }
            match world.get::<MassSystem>(entity) {
                Some(s) => Some(s.owner_id.clone()),
                None => {
                    let _ = tx_err.send(Err(anyhow::anyhow!(
                        "MassSystem missing on device {}",
                        addr_owned
                    )));
                    None
                }
            }
        })
        .flatten()
    })
    .await;

    let owner_id = match owner_id {
        Some(id) => id,
        None => return rx.await??,
    };

    let result = corelib::device::xiaomi::components::mass::send_file_for_owner(
        owner_id,
        data,
        data_type,
        move |d| (cb_arc)(d),
    )
    .await;
    let _ = tx.send(Ok(result.map(|_| ())));
    rx.await??;
    Ok(())
}

pub async fn forward_app_message(
    src_addr: &str,
    dst_addr: &str,
    package_name: &str,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    if src_addr == dst_addr {
        // #12 extra safety net: even callers who bypass relay_interconnect_message
        // shouldn't be able to self-forward and accidentally create a loop.
        return Err(anyhow::anyhow!(
            "[Transfer] forward_app_message: src and dst must differ (got {src_addr})"
        ));
    }

    info!(
        "[Transfer] Forwarding {} bytes from {} → {} (app: {})",
        payload.len(),
        src_addr,
        dst_addr,
        package_name
    );

    let app_info = resolve_app_info(dst_addr, package_name).await?;

    let dst_owned = dst_addr.to_string();
    let package_name_owned = package_name.to_string();
    let (tx, rx) = oneshot::channel();

    ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&dst_owned, |world, entity| {
            if world.get::<ThirdpartyAppComponent>(entity).is_none() {
                let _ = tx.send(Err(anyhow::anyhow!(
                    "ThirdpartyAppComponent missing on device {}",
                    dst_owned
                )));
                return;
            }
            let mut system = match world.get_mut::<ThirdpartyAppSystem>(entity) {
                Some(s) => s,
                None => {
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "ThirdpartyAppSystem missing on device {}",
                        dst_owned
                    )));
                    return;
                }
            };
            system.send_phone_message(&app_info, payload);
            info!(
                "[Transfer] Message forwarded to {} app on {}",
                package_name_owned, dst_owned
            );
            let _ = tx.send(Ok(()));
        });
    })
    .await;

    rx.await??;
    Ok(())
}

pub async fn relay_interconnect_message(
    src_addr: &str,
    dst_addr: &str,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    // #12: refuse identical endpoints — the relay task would emit a
    // CoreEvent::InterconnectMessage on the same device that it subscribes
    // to, creating a tight message loop until the heap fills.
    if src_addr == dst_addr {
        return Err(anyhow::anyhow!(
            "[Transfer] relay_interconnect_message: source and destination cannot be the same device ({src_addr})"
        ));
    }
    let src = src_addr.to_string();
    let dst = dst_addr.to_string();

    info!("[Transfer] Starting interconnect relay: {} → {}", src, dst);

    let mut subscriber = events::subscribe();

    // #12: track per-message (src pkg_name payload_hash) so a second relay
    // task that bridges dst back to src cannot echo the same payload again
    // within a bounded window. This defeats the "A↔B pingpong" loop even
    // when both endpoints spawn mutual relays.
    let recently_relayed: Arc<Mutex<HashSet<(String, String, u64)>>> =
        Arc::new(Mutex::new(HashSet::new()));
    let recently_relayed_drain = Arc::clone(&recently_relayed);
    // Periodically drain the dedupe window so memory use stays bounded.
    tokio::task::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            match recently_relayed_drain.lock() {
                Ok(mut set) if set.len() > 1024 => set.clear(),
                Ok(_) => {}
                Err(_p) => { /* mutex poisoned, we'll leak but keep running */ }
            }
        }
    });

    let handle = tokio::task::spawn(async move {
        while let Ok(event) = subscriber.recv().await {
            match event {
                events::CoreEvent::InterconnectMessage(msg) => {
                    if msg.device_addr == src {
                        let payload_hash = fnv_1a_64(&msg.payload);
                        let key = (msg.pkg_name.clone(), src.clone(), payload_hash);
                        let should_skip = match recently_relayed.lock() {
                            Ok(mut set) => !set.insert(key),
                            Err(_) => false,
                        };
                        if should_skip {
                            continue;
                        }
                        info!(
                            "[Transfer] Relaying message from {} (app: {}, {} bytes) → {}",
                            src,
                            msg.pkg_name,
                            msg.payload.len(),
                            dst
                        );
                        if let Err(err) =
                            forward_app_message(&src, &dst, &msg.pkg_name, msg.payload).await
                        {
                            warn!(
                                "[Transfer] Failed to relay message {} → {}: {err:?}",
                                src, dst
                            );
                        }
                    }
                }
                events::CoreEvent::DeviceStateChanged(state) => {
                    if state.device_addr == dst {
                        info!("[Transfer] Target device {} state changed", dst);
                    }
                }
            }
        }
    });

    Ok(handle)
}

fn fnv_1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

pub async fn transfer_quick_app_between_devices(
    src_addr: &str,
    dst_addr: &str,
    package_name: &str,
) -> anyhow::Result<()> {
    if src_addr == dst_addr {
        return Err(anyhow::anyhow!(
            "[Transfer] transfer_quick_app: source and destination must differ ({src_addr})"
        ));
    }
    info!(
        "[Transfer] Copying quick app {} from {} → {}",
        package_name, src_addr, dst_addr
    );

    let src_owned = src_addr.to_string();
    let pkg_owned = package_name.to_string();
    let (list_tx, list_rx) = oneshot::channel();

    ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&src_owned, |world, entity| {
            let component = match world.get::<ResourceComponent>(entity) {
                Some(c) => c,
                None => {
                    let _ = list_tx.send(Err(anyhow::anyhow!(
                        "ResourceComponent missing on source device {}",
                        src_owned
                    )));
                    return;
                }
            };
            let app_data = component
                .quick_apps
                .iter()
                .find(|item| item.package_name == pkg_owned)
                .cloned();
            match app_data {
                Some(app) => {
                    let _ = list_tx.send(Ok(app));
                }
                None => {
                    let _ = list_tx.send(Err(anyhow::anyhow!(
                        "App {} not found on source {}",
                        pkg_owned,
                        src_owned
                    )));
                }
            }
        });
    })
    .await;

    let app_item = list_rx.await??;

    // #15: previously we logged `app_item.package_name.len()` here, which
    // printed the *package name string length* instead of the actual binary
    // app payload size. For example a 500KB .pk package would look like
    // "30 bytes" in the log. Use the `data` field, unwrapped safely.
    info!(
        "[Transfer] Found app {} on {}, now installing on {} (payload relay unsupported)",
        package_name, src_addr, dst_addr
    );

    crate::install::install_quick_app(dst_addr, &app_item.package_name, Vec::new())
    .await?;

    info!(
        "[Transfer] Quick app {} successfully transferred {} → {}",
        package_name, src_addr, dst_addr
    );
    Ok(())
}

pub async fn transfer_watchface_between_devices(
    src_addr: &str,
    dst_addr: &str,
    watchface_id: &str,
) -> anyhow::Result<()> {
    info!(
        "[Transfer] Copying watchface {} from {} → {}",
        watchface_id, src_addr, dst_addr
    );

    let src_owned = src_addr.to_string();
    let id_owned = watchface_id.to_string();
    let (tx, rx) = oneshot::channel();

    ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&src_owned, |world, entity| {
            let component = match world
                .get::<corelib::device::xiaomi::components::resource::ResourceComponent>(
                entity,
            ) {
                Some(c) => c,
                None => {
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "ResourceComponent missing on source device {}",
                        src_owned
                    )));
                    return;
                }
            };
            let face_data = component
                .watchfaces
                .iter()
                .find(|w| w.id == id_owned)
                .cloned();
            match face_data {
                Some(face) => {
                    let _ = tx.send(Ok(face));
                }
                None => {
                    let _ = tx.send(Err(anyhow::anyhow!(
                        "Watchface {} not found on source {}",
                        id_owned,
                        src_owned
                    )));
                }
            }
        });
    })
    .await;

    let face_item = rx.await??;

    info!(
        "[Transfer] Found watchface {} on {}, now installing on {}",
        watchface_id, src_addr, dst_addr
    );

    let _ = face_item.id.clone();
    crate::install::install_watchface(dst_addr, Vec::new()).await?;

    info!(
        "[Transfer] Watchface {} successfully transferred {} → {}",
        watchface_id, src_addr, dst_addr
    );
    Ok(())
}

pub async fn broadcast_data_to_all_devices(
    data_type: MassDataType,
    data: Vec<u8>,
) -> anyhow::Result<Vec<(String, anyhow::Result<()>)>> {
    let device_ids = ecs::with_rt_mut(|rt| rt.device_ids().cloned().collect::<Vec<_>>()).await;

    if device_ids.is_empty() {
        return Ok(vec![]);
    }

    info!(
        "[Transfer] Broadcasting {} bytes to {} device(s)",
        data.len(),
        device_ids.len()
    );

    let mut results = Vec::new();
    for addr in &device_ids {
        let result = send_data_to_device(addr, data_type, data.clone()).await;
        results.push((addr.clone(), result));
        match results.last() {
            Some((a, Ok(_))) => info!("[Transfer] Broadcast to {} OK", a),
            Some((a, Err(e))) => warn!("[Transfer] Broadcast to {} failed: {e:?}", a),
            None => {}
        }
    }

    Ok(results)
}

pub async fn list_connected_devices() -> Vec<String> {
    ecs::with_rt_mut(|rt| rt.device_ids().cloned().collect::<Vec<_>>()).await
}

pub async fn get_device_info(addr: &str) -> anyhow::Result<String> {
    let addr_owned = addr.to_string();
    let (tx, rx) = oneshot::channel();

    ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_owned, |world, entity| {
            let name = world
                .get::<corelib::device::xiaomi::XiaomiDevice>(entity)
                .map(|d| d.name().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            let _ = tx.send(Ok(name));
        });
    })
    .await;

    rx.await?
}

async fn resolve_app_info(addr: &str, package_name: &str) -> anyhow::Result<AppInfo> {
    crate::install::resolve_app_info(addr, package_name).await
}
