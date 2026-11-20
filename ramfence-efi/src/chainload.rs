//! Find and start the next boot stage, normally bootmgfw.efi.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use uefi::boot::{self, LoadImageSource, OpenProtocolAttributes, OpenProtocolParams};
use uefi::fs::FileSystem;
use uefi::proto::BootPolicy;
use uefi::proto::device_path::build::{self, DevicePathBuilder};
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CStr16, CString16, Handle, Status};

use crate::console::Log;

fn file_exists(handle: Handle, path: &CStr16) -> bool {
    match boot::open_protocol_exclusive::<SimpleFileSystem>(handle) {
        Ok(sfs) => FileSystem::new(sfs).try_exists(path).unwrap_or(false),
        Err(_) => false,
    }
}

/// Device path of `handle` followed by a file path node for `path`. A full
/// path lets bootmgr find its own partition (and BCD) via LoadedImage.
fn full_device_path(handle: Handle, path: &CStr16) -> Result<Box<DevicePath>, String> {
    // GetProtocol: read-only peek, does not disturb drivers using the handle.
    let dev = unsafe {
        boot::open_protocol::<DevicePath>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .map_err(|e| format!("cannot open device path: {:?}", e.status()))?;

    let mut buf = Vec::new();
    let mut builder = DevicePathBuilder::with_vec(&mut buf);
    for node in dev.node_iter() {
        builder = builder
            .push(&node)
            .map_err(|e| format!("device path build error: {e:?}"))?;
    }
    let full = builder
        .push(&build::media::FilePath { path_name: path })
        .and_then(|b| b.finalize())
        .map_err(|e| format!("device path build error: {e:?}"))?;
    Ok(full.to_boxed())
}

/// Volumes to search: our own first, then every other FAT volume, so a USB
/// test copy can start Windows from the internal disk.
fn candidate_volumes() -> Vec<Handle> {
    let own = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .ok()
        .and_then(|li| li.device());
    let mut list: Vec<Handle> = own.into_iter().collect();
    if let Ok(all) = boot::find_handles::<SimpleFileSystem>() {
        list.extend(all.into_iter().filter(|h| Some(*h) != own));
    }
    list
}

/// Loads and starts `path`. Only returns on failure.
pub fn boot(path: &str, log: &mut Log) -> Result<(), String> {
    let path16 = CString16::try_from(path).map_err(|_| format!("invalid chainload path '{path}'"))?;

    let volumes = candidate_volumes();
    let Some((index, handle)) = volumes
        .iter()
        .enumerate()
        .find(|(_, h)| file_exists(**h, &path16))
    else {
        return Err(format!(
            "{path} not found on any of {} FAT volume(s)",
            volumes.len()
        ));
    };
    log.info(format_args!(
        "Starting {path} (volume {}{})",
        index,
        if index == 0 { ", same as ramfence.efi" } else { "" }
    ));

    let dp = full_device_path(*handle, &path16)?;
    let child = boot::load_image(
        boot::image_handle(),
        LoadImageSource::FromDevicePath {
            device_path: &dp,
            boot_policy: BootPolicy::ExactMatch,
        },
    )
    .map_err(|e| {
        let hint = match e.status() {
            Status::SECURITY_VIOLATION | Status::ACCESS_DENIED => {
                " (rejected by Secure Boot?)"
            }
            _ => "",
        };
        format!("LoadImage {path} failed: {:?}{hint}", e.status())
    })?;

    match boot::start_image(child) {
        Ok(()) => Err(format!("{path} exited without booting")),
        Err(e) => Err(format!("{path} failed: {:?}", e.status())),
    }
}
