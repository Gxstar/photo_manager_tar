use crate::models::Device;
use std::path::Path;

/// 相机卡特征目录（命中其一即判定为相机卡）
const CAMERA_DIR_MARKERS: &[&str] = &[
    "DCIM",
    "PRIVATE",
    "M4HD",
    "100PANASONIC",
    "101_PANA",
    "100CANON",
    "100NIKON",
    "100MSDCF",
];

/// 枚举所有可移动卷（DriveType == DRIVE_REMOVABLE），并做相机卡判定
pub fn list_devices() -> Vec<Device> {
    let mut devices = Vec::new();
    for root in list_removable_roots() {
        let root_path = Path::new(&root);
        let (total, free) = disk_space(&root).unwrap_or((0, 0));
        let has_media = total > 0;
        let (is_camera, kind) = if has_media {
            detect_camera_card(root_path)
        } else {
            (false, None)
        };

        devices.push(Device {
            path: root.clone(),
            label: volume_label(root_path).unwrap_or_default(),
            total_bytes: total,
            free_bytes: free,
            has_media,
            is_camera_card: is_camera,
            card_kind: kind,
        });
    }
    devices
}

/// 枚举可移动盘符（A: ~ Z:）
fn list_removable_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

        const DRIVE_REMOVABLE: u32 = 2; // Win32 DRIVE_TYPE：2 = 可移动磁盘

        let mut roots = Vec::new();
        for i in b'A'..=b'Z' {
            let root = format!("{}:\\", i as char);
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            let t = unsafe { GetDriveTypeW(wide.as_ptr()) };
            if t == DRIVE_REMOVABLE {
                roots.push(root);
            }
        }
        roots
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// (总容量, 剩余容量)
fn disk_space(root: &str) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        let mut avail: u64 = 0;
        let mut total: u64 = 0;
        let mut free: u64 = 0;
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
        if ok != 0 {
            Some((total, avail))
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
    }
}

/// 卷标（读不到返回空字符串，不影响功能）
fn volume_label(root: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root_w: Vec<u16> = root
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut name_buf = [0u16; 128];
        let ok = unsafe {
            GetVolumeInformationW(
                root_w.as_ptr(),
                name_buf.as_mut_ptr(),
                name_buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok != 0 {
            let end = name_buf.iter().position(|&c| c == 0).unwrap_or(0);
            let label = String::from_utf16_lossy(&name_buf[..end]);
            Some(label.trim().to_string())
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
    }
}

/// 指定盘符的剩余空间（供扫描器做空间预检）
pub fn free_space_of_drive(root: &str) -> Option<u64> {
    disk_space(root).map(|(_, free)| free)
}

/// 相机卡判定：根目录下命中特征目录
fn detect_camera_card(root: &Path) -> (bool, Option<String>) {
    for marker in CAMERA_DIR_MARKERS {
        if root.join(marker).is_dir() {
            return (true, Some(marker.to_string()));
        }
    }
    (false, None)
}
