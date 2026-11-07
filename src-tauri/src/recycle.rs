//! 把库内媒体文件移入 **Windows 系统回收站**（可还原），绝不做永久删除。
//!
//! 这是**当前**唯一会移除库内文件的入口；docs §16 把只读收窄为导入阶段后，
//! 不再意味着「只允许存在一个入口」，只是本轮只实现了这一条。
//! 调用方 `gallery_delete_rejected` 必须先完成「仅处理已标记的废片 +
//! 路径在媒体库根内 + 前端列清单二次确认」三道校验。
//! 不做批量静默删除、不碰 SD 卡、不提供回收站还原界面（用系统回收站自带的还原）。

use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
    SHFileOperationW,
};

/// `pFrom` 必须是双重 NUL 结尾的宽字符路径列表，单文件也要补两个 0。
/// `FOF_ALLOWUNDO` 是进回收站的关键，缺了它就成了永久删除。
pub fn send_to_recycle_bin(path: &Path) -> Result<(), String> {
    let wide: Vec<u16> = path
        .to_string_lossy()
        .replace('/', "\\")
        .encode_utf16()
        .chain([0u16, 0u16])
        .collect();

    let mut op = SHFILEOPSTRUCTW::default();
    op.wFunc = FO_DELETE;
    op.pFrom = PCWSTR(wide.as_ptr());
    op.fFlags =
        (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16;

    // SAFETY: wide 在调用期间存活且以双 NUL 结尾；op 其余指针字段保持 null，
    // hwnd 为 null 表示不弹系统确认框（确认已在前端复核清单里完成）。
    let rc = unsafe { SHFileOperationW(&mut op as *mut SHFILEOPSTRUCTW) };
    if rc != 0 {
        return Err(format!("移入回收站失败（系统代码 {rc}）"));
    }
    // 系统返回 0 也可能什么都没做（操作被中止），以文件是否还在原位为准
    if path.exists() {
        return Err("文件仍在原位置，未移入回收站".into());
    }
    Ok(())
}
