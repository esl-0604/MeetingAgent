//! Native Windows folder-picker via the modern `IFileOpenDialog` API.
//!
//! Pre-fills the dialog's file-name edit box with a sensible default and uses
//! an `IFileDialogEvents` callback to keep the dialog open if the user clicks
//! "선택" while the name box is empty.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use windows::core::{implement, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, S_FALSE};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileDialog, IFileDialogEvents, IFileDialogEvents_Impl, IFileOpenDialog,
    IShellItem, SHCreateItemFromParsingName, FDE_OVERWRITE_RESPONSE, FDE_SHAREVIOLATION_RESPONSE,
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONWARNING, MB_OK,
};

/// Show a modal folder-picker. Returns `Ok(None)` if the user cancels.
pub fn pick_folder(title: &str, default_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    pick_folder_with_owner(title, default_dir, None)
}

/// Like `pick_folder` but lets the caller specify an owner HWND so the
/// dialog appears modal to a specific window (e.g. the welcome window stays
/// visible while the dialog is shown on top of it).
///
/// The "File name" edit box is pre-filled with the default folder's path.
/// If the user clears it and clicks "선택", we intercept the OK via an
/// `IFileDialogEvents` callback, show a warning MessageBox, and keep the
/// dialog open — Windows's default behaviour is to silently accept the
/// current view, which the user found confusing.
pub fn pick_folder_with_owner(
    title: &str,
    default_dir: Option<&Path>,
    owner: Option<HWND>,
) -> Result<Option<PathBuf>> {
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
        // Pre-fill the "File name" edit control with the default path so
        // the user can either keep it (Enter) or replace it.
        let path_w: HSTRING = d.to_string_lossy().as_ref().into();
        let _ = unsafe { dialog.SetFileName(&path_w) };
    }

    let owner_hwnd = owner.unwrap_or(HWND(std::ptr::null_mut()));

    // Wire up the OnFileOk validator so an empty file-name field doesn't
    // silently close the dialog.
    let dlg_iface: IFileDialog = dialog.cast().context("IFileOpenDialog → IFileDialog cast")?;
    let events_obj: IFileDialogEvents = PickerEvents { owner: owner_hwnd }.into();
    let cookie = unsafe { dlg_iface.Advise(&events_obj) }
        .context("IFileDialog::Advise")?;

    // Show is modal. Cancel returns ERROR_CANCELLED (0x800704C7).
    let show_result = unsafe { dialog.Show(owner_hwnd) };
    let _ = unsafe { dlg_iface.Unadvise(cookie) };

    match show_result {
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

#[implement(IFileDialogEvents)]
struct PickerEvents {
    owner: HWND,
}

impl IFileDialogEvents_Impl for PickerEvents_Impl {
    fn OnFileOk(&self, pfd: Option<&IFileDialog>) -> windows::core::Result<()> {
        let Some(dialog) = pfd else {
            return Ok(());
        };
        // Read the current text in the file-name edit control. Empty (or
        // whitespace-only) → reject with a warning + return S_FALSE so the
        // dialog stays open.
        let name_pwstr = unsafe { dialog.GetFileName()? };
        let name_str = unsafe { pwstr_to_string(name_pwstr.0) };
        unsafe { CoTaskMemFree(Some(name_pwstr.0 as *const _ as *const _)) };

        if name_str.trim().is_empty() {
            let title_w: HSTRING = "폴더를 선택해주세요".into();
            let body_w: HSTRING =
                "파일 이름란이 비어 있습니다.\n저장할 폴더 경로를 입력하거나 선택해주세요."
                    .into();
            unsafe {
                MessageBoxW(
                    self.owner,
                    PCWSTR(body_w.as_ptr()),
                    PCWSTR(title_w.as_ptr()),
                    MB_OK | MB_ICONWARNING,
                );
            }
            // S_FALSE keeps the dialog open.
            return Err(windows::core::Error::from_hresult(S_FALSE));
        }
        Ok(())
    }

    fn OnFolderChanging(
        &self,
        _: Option<&IFileDialog>,
        _: Option<&IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnFolderChange(&self, _: Option<&IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSelectionChange(&self, _: Option<&IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnShareViolation(
        &self,
        _: Option<&IFileDialog>,
        _: Option<&IShellItem>,
    ) -> windows::core::Result<FDE_SHAREVIOLATION_RESPONSE> {
        Ok(FDE_SHAREVIOLATION_RESPONSE(0))
    }
    fn OnTypeChange(&self, _: Option<&IFileDialog>) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnOverwrite(
        &self,
        _: Option<&IFileDialog>,
        _: Option<&IShellItem>,
    ) -> windows::core::Result<FDE_OVERWRITE_RESPONSE> {
        Ok(FDE_OVERWRITE_RESPONSE(0))
    }
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
