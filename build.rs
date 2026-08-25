use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    emit_priv_cfg_flag();
    emit_build_time_env();

    embuild::espidf::sysenv::output();
}

fn emit_priv_cfg_flag() {
    if let Some(marker) = find_existing_marker() {
        println!("cargo:rerun-if-changed={}", marker.display());
        println!("cargo:rustc-cfg=astrobox_priv_cloned");
    } else {
        println!("cargo:rerun-if-env-changed=ASTROBOX_PRIV_CLONED");
    }
}

fn find_existing_marker() -> Option<PathBuf> {
    priv_marker_candidates()
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn priv_marker_candidates() -> Vec<PathBuf> {
    let mut markers = Vec::new();
    let mut current = workspace_dir();
    for _ in 0..4 {
        let Some(dir) = current.clone() else {
            break;
        };
        markers.push(dir.join("__PRIV_CLONED"));
        current = dir.parent().map(|p| p.to_path_buf());
    }
    markers
}

fn workspace_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("CARGO_WORKSPACE_DIR") {
        return Some(PathBuf::from(dir));
    }
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    manifest_dir.ancestors().nth(2).map(|p| p.to_path_buf())
}

/// Emit `cargo:rustc-env=BUILD_TIME=2026-08-24T10:00:00Z` so
/// `env!("BUILD_TIME")` in the firmware / web UI returns the build stamp.
/// Re-runs whenever build.rs itself changes (cargo handles rerun automatically
/// for build.rs edits; build-time stamp naturally depends on build instant).
fn emit_build_time_env() {
    // Prefer RFC3339-formatted UTC time (chrono free; std only)
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    // Convert unix timestamp to Y-M-D H:M:S UTC using known formula
    let (y, mo, d, h, mi, s) = secs_to_ymdhms(secs);
    let stamp = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
    println!("cargo:rustc-env=BUILD_TIME={stamp}");
}

fn secs_to_ymdhms(mut secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let s = secs.rem_euclid(60) as u32;
    secs = secs.div_euclid(60);
    let mi = secs.rem_euclid(60) as u32;
    secs = secs.div_euclid(60);
    let h = secs.rem_euclid(24) as u32;
    let mut days: i64 = secs.div_euclid(24);
    // 1970-01-01 is epoch. Iterate years/months.
    let mut year: i32 = 1970;
    loop {
        let leap = is_leap(year);
        let yd = if leap { 366 } else { 365 };
        if days >= yd as i64 {
            days -= yd as i64;
            year += 1;
        } else {
            break;
        }
    }
    let md: [u32; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month: u32 = 1;
    for m in md.iter() {
        let ml = *m as i64;
        if days >= ml {
            days -= ml;
            month += 1;
        } else {
            break;
        }
    }
    (year, month, days as u32 + 1, h, mi, s)
}
const fn is_leap(y: i32) -> bool {
    if y % 4 != 0 {
        return false;
    }
    if y % 100 != 0 {
        return true;
    }
    y % 400 == 0
}
