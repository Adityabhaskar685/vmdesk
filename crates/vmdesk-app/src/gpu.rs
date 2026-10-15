//! Keeps the GPU drivers the app never uses out of memory.
//!
//! GPUI's renderer enumerates every graphics backend before choosing one. On Mesa that
//! loads the software Vulkan driver (llvmpipe) and the EGL/GL stack, which together pull
//! in `libgallium` and all of `libLLVM`: about 55 MB, for a window that then renders on
//! the hardware Vulkan driver anyway. Both loaders can be told to skip them, but only
//! through the environment and before they load, so the app re-executes itself once with
//! the variables set (setting them in-process would need `unsafe`).

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Set on the re-executed process, so it happens once. Its value lists the variables the
/// app added, so programs it starts (the terminal) can be given the environment back.
const DONE: &str = "VMDESK_GPU_TRIMMED";
/// Set to keep every driver, e.g. on a machine whose only Vulkan driver is llvmpipe.
pub const OPT_OUT: &str = "VMDESK_ALL_GPU_DRIVERS";

/// PCI vendor id → Vulkan drivers (loader globs) for that vendor's GPUs.
const VENDOR_DRIVERS: [(&str, &str); 4] = [
    ("0x8086", "*intel*"),
    ("0x1002", "*radeon*"),
    ("0x10de", "*nvidia*,*nouveau*"),
    ("0x1af4", "*virtio*"),
];

/// Vulkan drivers for the GPUs actually present, from the DRM devices in sysfs. `None`
/// when no known GPU is found, so every driver stays available.
fn present_gpu_drivers() -> Option<String> {
    let mut found: Vec<&str> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("device/vendor")).ok())
        .filter_map(|v| {
            VENDOR_DRIVERS
                .iter()
                .find(|(id, _)| v.trim() == *id)
                .map(|(_, d)| *d)
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    (!found.is_empty()).then(|| found.join(","))
}

/// Re-executes the process with the drivers trimmed, unless that already happened, the
/// user opted out, or every variable is already set. Returns only if no re-exec happened.
pub fn trim_drivers() {
    if std::env::var_os(DONE).is_some() || std::env::var_os(OPT_OUT).is_some() {
        return;
    }
    // Only the present GPUs' Vulkan drivers (never llvmpipe or another vendor's, some of
    // which link LLVM), and no EGL vendors, so the GL backend is not probed.
    let mut trim = vec![("__EGL_VENDOR_LIBRARY_DIRS", "/nonexistent".to_owned())];
    match present_gpu_drivers() {
        Some(drivers) => trim.push(("VK_LOADER_DRIVERS_SELECT", drivers)),
        None => trim.push(("VK_LOADER_DRIVERS_DISABLE", "*lvp*".to_owned())),
    }
    let missing: Vec<_> = trim
        .into_iter()
        .filter(|(k, _)| std::env::var_os(k).is_none())
        .collect();
    if missing.is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let added: Vec<&str> = missing.iter().map(|(k, _)| *k).collect();
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(DONE, added.join(","));
    for (k, v) in missing {
        cmd.env(k, v);
    }
    // Only returns on failure; then carry on with every driver.
    let err = cmd.exec();
    tracing::debug!(error = %err, "could not re-exec with trimmed GPU drivers");
}

/// Removes the variables [`trim_drivers`] added (and its marker) from a child's
/// environment. Without this a terminal started by the app inherits "no GL drivers" and
/// cannot create its OpenGL context.
pub fn restore_child_env(cmd: &mut Command) {
    let Some(added) = std::env::var_os(DONE) else {
        return;
    };
    for name in added_names(&added.to_string_lossy()) {
        cmd.env_remove(name);
    }
    cmd.env_remove(DONE);
}

fn added_names(marker: &str) -> impl Iterator<Item = &str> {
    marker.split(',').filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn marker_lists_what_to_give_back() {
        let names: Vec<&str> =
            super::added_names("__EGL_VENDOR_LIBRARY_DIRS,VK_LOADER_DRIVERS_SELECT").collect();
        assert_eq!(
            names,
            ["__EGL_VENDOR_LIBRARY_DIRS", "VK_LOADER_DRIVERS_SELECT"]
        );
        assert_eq!(super::added_names("").count(), 0);
    }
}
