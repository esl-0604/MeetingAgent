//! Tray icon + dynamic menu construction.
//!
//! The menu reflects the current `GuiState`: items are enabled/disabled by
//! rebuilding the menu wholesale each time state changes (cheap and avoids
//! the brittle "find item by id" dance).

use muda::{
    AboutMetadata, CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder};

use super::{icons, GuiState};

/// Stable menu IDs. We compare against these in the event handler.
pub const ID_STATUS: &str = "status";
pub const ID_START_RECORDING: &str = "start_recording";
pub const ID_STOP_RECORDING: &str = "stop_recording";
pub const ID_OPEN_LAST_SESSION: &str = "open_last_session";
pub const ID_CHANGE_SAVE_DIR: &str = "change_save_dir";
pub const ID_TOGGLE_AUTO_RECORD: &str = "toggle_auto_record";
pub const ID_TOGGLE_AUTO_START: &str = "toggle_auto_start";
pub const ID_TOGGLE_EVENT_NOTIFS: &str = "toggle_event_notifs";
pub const ID_OPEN_LOG: &str = "open_log";
pub const ID_QUIT: &str = "quit";

pub struct TrayHandle {
    pub icon: TrayIcon,
    /// Held alive — these are the per-build menu items. Replaced every rebuild.
    _menu: Menu,
}

pub struct MenuFlags {
    pub auto_record: bool,
    pub auto_start: bool,
    pub event_notifications: bool,
    pub last_session: Option<std::path::PathBuf>,
}

pub fn build_initial(state: &GuiState, flags: &MenuFlags) -> TrayHandle {
    let menu = build_menu(state, flags);
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip(tooltip(state))
        .with_icon(icon_for(state))
        .build()
        .expect("tray icon build");
    TrayHandle { icon, _menu: menu }
}

pub fn rebuild(handle: &mut TrayHandle, state: &GuiState, flags: &MenuFlags) {
    let menu = build_menu(state, flags);
    let _ = handle.icon.set_menu(Some(Box::new(menu.clone())));
    let _ = handle.icon.set_tooltip(Some(tooltip(state)));
    let _ = handle.icon.set_icon(Some(icon_for(state)));
    handle._menu = menu;
}

fn icon_for(state: &GuiState) -> tray_icon::Icon {
    match state {
        GuiState::Idle => icons::idle_icon(),
        GuiState::Detected { .. } | GuiState::Ignored { .. } | GuiState::Missed { .. } => {
            icons::detected_icon()
        }
        GuiState::Recording
        | GuiState::AwaitingEndConfirm
        | GuiState::Finalizing { .. } => icons::recording_icon(),
    }
}

fn tooltip(state: &GuiState) -> String {
    match state {
        GuiState::Idle => "Meeting Agent — 미팅 대기 중".into(),
        GuiState::Detected { .. } => "Meeting Agent — 미팅 감지됨 (응답 대기)".into(),
        GuiState::Ignored { .. } => "Meeting Agent — 미팅 진행 중 (자동 알림 비활성)".into(),
        GuiState::Missed { .. } => "Meeting Agent — 미팅 진행 중 (응답 시간 초과)".into(),
        GuiState::Recording => "Meeting Agent — 녹화 중".into(),
        GuiState::AwaitingEndConfirm => "Meeting Agent — 미팅 종료 대기".into(),
        GuiState::Finalizing { .. } => "Meeting Agent — 마무리 중…".into(),
    }
}

fn build_menu(state: &GuiState, flags: &MenuFlags) -> Menu {
    let menu = Menu::new();

    let status_text = format!("● {}", status_label(state));
    let status = MenuItem::with_id(MenuId::new(ID_STATUS), status_text, false, None);
    let _ = menu.append(&status);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let can_start = matches!(
        state,
        GuiState::Detected { .. } | GuiState::Ignored { .. } | GuiState::Missed { .. }
    );
    let can_stop = matches!(state, GuiState::Recording | GuiState::AwaitingEndConfirm);

    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(ID_START_RECORDING),
        "▶  녹화 시작",
        can_start,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(ID_STOP_RECORDING),
        "■  녹화 중지",
        can_stop,
        None,
    ));
    let _ = menu.append(&PredefinedMenuItem::separator());

    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(ID_OPEN_LAST_SESSION),
        "📁  마지막 세션 폴더 열기",
        flags.last_session.is_some(),
        None,
    ));
    let _ = menu.append(&PredefinedMenuItem::separator());

    let settings = Submenu::new("⚙  설정", true);
    let _ = settings.append(&MenuItem::with_id(
        MenuId::new(ID_CHANGE_SAVE_DIR),
        "저장 위치 변경…",
        true,
        None,
    ));
    let _ = settings.append(&CheckMenuItem::with_id(
        MenuId::new(ID_TOGGLE_AUTO_RECORD),
        "감지 시 자동 녹화 (묻지 않음)",
        true,
        flags.auto_record,
        None,
    ));
    let _ = settings.append(&CheckMenuItem::with_id(
        MenuId::new(ID_TOGGLE_AUTO_START),
        "Windows 시작 시 자동 실행",
        true,
        flags.auto_start,
        None,
    ));
    let _ = settings.append(&CheckMenuItem::with_id(
        MenuId::new(ID_TOGGLE_EVENT_NOTIFS),
        "이벤트 알림 (자막/공유/리사이즈 등)",
        true,
        flags.event_notifications,
        None,
    ));
    let _ = settings.append(&PredefinedMenuItem::separator());
    let _ = settings.append(&MenuItem::with_id(
        MenuId::new(ID_OPEN_LOG),
        "로그 파일 보기 (agent.log)",
        true,
        None,
    ));
    let _ = menu.append(&settings);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let _ = menu.append(&PredefinedMenuItem::about(
        Some("정보"),
        Some(AboutMetadata {
            name: Some("Meeting Agent".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            authors: Some(vec!["eunsang.lee@endorobo.com".into()]),
            comments: Some("Teams 미팅 자동 캡처 도구".into()),
            ..Default::default()
        }),
    ));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(MenuId::new(ID_QUIT), "✕  종료", true, None));

    menu
}

fn status_label(state: &GuiState) -> &'static str {
    match state {
        GuiState::Idle => "미팅 대기 중",
        GuiState::Detected { .. } => "미팅 감지됨 (응답 대기)",
        GuiState::Ignored { .. } => "미팅 진행 중 (무시됨)",
        GuiState::Missed { .. } => "미팅 진행 중 (응답 없음)",
        GuiState::Recording => "녹화 중",
        GuiState::AwaitingEndConfirm => "미팅 종료 대기",
        GuiState::Finalizing { .. } => "마무리 중…",
    }
}
