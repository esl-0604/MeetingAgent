//! UI Automation helpers.
//!
//! UIA exposes the live accessibility tree of every running app. New Teams is
//! a WebView2 host, so its DOM is reflected verbatim into this tree — meaning
//! reading caption text via UIA is **not** OCR; we are reading the same string
//! Teams' web app rendered. This is the deepest data layer available to a
//! non-injected, non-MITM observer.

pub mod captions;

use anyhow::{Context, Result};
use windows::core::BSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};

/// Per-thread COM init — call from any worker thread that touches COM.
pub fn com_init_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

pub fn create_automation() -> Result<IUIAutomation> {
    com_init_thread();
    let auto: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .context("CoCreateInstance(CUIAutomation)")?;
    Ok(auto)
}

pub fn element_from_hwnd(auto: &IUIAutomation, hwnd: isize) -> Result<IUIAutomationElement> {
    let elem = unsafe { auto.ElementFromHandle(HWND(hwnd as _)) }
        .with_context(|| format!("ElementFromHandle({hwnd:#x})"))?;
    Ok(elem)
}

/// Read `CurrentName` as a `String`, returning `""` for empty BSTRs.
pub fn name_of(elem: &IUIAutomationElement) -> String {
    bstr_to_string(unsafe { elem.CurrentName() }.unwrap_or_default())
}

pub fn help_text_of(elem: &IUIAutomationElement) -> String {
    bstr_to_string(unsafe { elem.CurrentHelpText() }.unwrap_or_default())
}

pub fn class_of(elem: &IUIAutomationElement) -> String {
    bstr_to_string(unsafe { elem.CurrentClassName() }.unwrap_or_default())
}

pub fn automation_id_of(elem: &IUIAutomationElement) -> String {
    bstr_to_string(unsafe { elem.CurrentAutomationId() }.unwrap_or_default())
}

pub fn control_type_of(elem: &IUIAutomationElement) -> i32 {
    unsafe { elem.CurrentControlType() }.map(|c| c.0).unwrap_or(0)
}

pub fn localized_type_of(elem: &IUIAutomationElement) -> String {
    bstr_to_string(unsafe { elem.CurrentLocalizedControlType() }.unwrap_or_default())
}

pub fn is_offscreen(elem: &IUIAutomationElement) -> bool {
    unsafe { elem.CurrentIsOffscreen() }.map(|b| b.as_bool()).unwrap_or(false)
}

pub fn bstr_to_string(b: BSTR) -> String {
    b.to_string()
}

/// Walk all descendants of `root` (BFS, depth-limited) emitting visit callbacks.
///
/// `skip_offscreen` controls whether subtrees with `IsOffscreen=true` are
/// pruned. **Set this to `false` when reading captions during screen-share** —
/// Teams marks the captions panel offscreen when the presentation view
/// covers it, but the DOM nodes (and their text) are still alive and readable.
pub fn walk_descendants<F: FnMut(&IUIAutomationElement, u32) -> WalkAction>(
    auto: &IUIAutomation,
    root: &IUIAutomationElement,
    max_depth: u32,
    skip_offscreen: bool,
    mut visit: F,
) -> Result<()> {
    let walker = unsafe { auto.ControlViewWalker() }.context("ControlViewWalker")?;
    let mut stack: Vec<(IUIAutomationElement, u32)> = Vec::new();
    stack.push((root.clone(), 0));
    while let Some((node, depth)) = stack.pop() {
        match visit(&node, depth) {
            WalkAction::Stop => return Ok(()),
            WalkAction::SkipChildren => continue,
            WalkAction::Continue => {}
        }
        if depth >= max_depth {
            continue;
        }
        // Enumerate children via the control-view walker — safer than FindAll for huge trees.
        let mut child = unsafe { walker.GetFirstChildElement(&node) }.ok();
        while let Some(c) = child {
            if !skip_offscreen || !is_offscreen(&c) {
                stack.push((c.clone(), depth + 1));
            }
            child = unsafe { walker.GetNextSiblingElement(&c) }.ok();
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum WalkAction {
    Continue,
    SkipChildren,
    Stop,
}

/// Find the first descendant whose `Name` contains any of the given needles
/// (case-insensitive). Useful for locating caption containers across UI
/// language and Teams version variations.
pub fn find_descendant_by_name_contains(
    auto: &IUIAutomation,
    root: &IUIAutomationElement,
    needles: &[&str],
    max_depth: u32,
) -> Result<Option<IUIAutomationElement>> {
    let mut hit: Option<IUIAutomationElement> = None;
    // Don't skip offscreen — caption containers can be hidden behind the
    // presentation view but still hold live caption text.
    walk_descendants(auto, root, max_depth, false, |e, _| {
        let n = name_of(e).to_lowercase();
        if needles.iter().any(|nd| n.contains(&nd.to_lowercase())) {
            hit = Some(e.clone());
            WalkAction::Stop
        } else {
            WalkAction::Continue
        }
    })?;
    Ok(hit)
}

/// Read all "leaf" text under an element by collecting Names of Text controls
/// and Edit controls in document order.
pub fn collect_text(auto: &IUIAutomation, root: &IUIAutomationElement, max_depth: u32) -> Result<String> {
    let mut out = String::new();
    // Always include offscreen text — caption rows are routinely offscreen
    // during presentation view.
    walk_descendants(auto, root, max_depth, false, |e, _| {
        let ct = control_type_of(e);
        // 50020 = UIA_TextControlTypeId, 50004 = UIA_EditControlTypeId
        if ct == 50020 || ct == 50004 {
            let n = name_of(e);
            if !n.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&n);
            }
        }
        WalkAction::Continue
    })?;
    Ok(out)
}

/// Convenience for the `--uia-dump` debug path: print the visible UIA tree.
#[allow(dead_code)]
pub fn dump_tree(auto: &IUIAutomation, root: &IUIAutomationElement, max_depth: u32) -> Result<()> {
    walk_descendants(auto, root, max_depth, false, |e, depth| {
        let pad = "  ".repeat(depth as usize);
        println!(
            "{pad}[{ct}] name={:?} class={:?} aid={:?} loc={:?}",
            name_of(e),
            class_of(e),
            automation_id_of(e),
            localized_type_of(e),
            ct = control_type_of(e),
        );
        WalkAction::Continue
    })
}
