//! Native Windows folder-picker via the modern `IFileOpenDialog` API.
//!
//! Used at session end (when `prompt_save_on_exit` is set) so the user can
//! send the session directory to a chosen archive location instead of the
//! default Documents folder.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM,
    FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};

/// Show a modal folder-picker. Returns `Ok(None)` if the user cancels.
///
/// `default_dir`, if `Some`, becomes the dialog's initial directory.
pub fn pick_folder(title: &str, default_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    crate::uia::com_init_thread();

    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
            .context("CoCreateInstance(FileOpenDialog)")?;

    let title_w: HSTRING = title.into();
    unsafe { dialog.SetTitle(&title_w) }.context("SetTitle")?;

    let current_opts = unsafe { dialog.GetOptions() }.context("GetOptions")?;
    unsafe {
        dialog.SetOptions(current_opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
    }
    .context("SetOptions")?;

    if let Some(d) = default_dir {
        if let Ok(item) = shell_item_from_path(d) {
            // SetDefaultFolder is preferred; SetFolder forces, ignoring saved state.
            let _ = unsafe { dialog.SetDefaultFolder(&item) };
        }
    }

    // Show is modal. Cancel returns ERROR_CANCELLED (0x800704C7).
    match unsafe { dialog.Show(HWND(std::ptr::null_mut())) } {
        Ok(()) => {}
        Err(e) => {
            if e.code().0 as u32 == 0x800704C7 {
                return Ok(None);
            }
            return Err(anyhow::anyhow!("FileOpenDialog::Show: {e}"));
        }
    }

    let item: IShellItem = unsafe { dialog.GetResult() }.context("GetResult")?;
    let pwstr = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.context("GetDisplayName")?;
    let path = unsafe { pwstr_to_string(pwstr.0) };
    unsafe { CoTaskMemFree(Some(pwstr.0 as *const _ as *const _)) };

    Ok(Some(PathBuf::from(path)))
}

fn shell_item_from_path(path: &Path) -> Result<IShellItem> {
    let path_str = path.to_string_lossy();
    let h: HSTRING = path_str.as_ref().into();
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(h.as_ptr()), None) }.context("SHCreateItemFromParsingName")?;
    Ok(item)
}

unsafe fn pwstr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}
