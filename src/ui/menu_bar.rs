// many ideas for how this works were taken from https://github.com/xiamaz/YabaiIndicator
use std::cell::RefCell;
use std::collections::HashMap;

use regex::RegexBuilder;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{ClassType, DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSColor, NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext, NSMenu, NSMenuItem,
    NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSView,
};
use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CGFloat, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{CGBlendMode, CGContext};
use objc2_core_text::CTLine;
use objc2_foundation::{
    MainThreadMarker, NSAttributedStringKey, NSDictionary, NSMutableDictionary, NSObject, NSRect,
    NSSize, NSString,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use crate::actor::reactor::{Command as ReactorTopCommand, ReactorCommand};
use crate::actor::wm_controller::{WmCmd, WmCommand};
use crate::common::config::{
    ActiveWorkspaceLabel, LayoutMode, MenuBarDisplayMode, MenuBarSettings,
    MenuBarWorkspaceActivityRule, MenuBarWorkspaceActivityState, WorkspaceDisplayStyle,
    WorkspaceSelector,
};
use crate::layout_engine::LayoutCommand;
use crate::model::VirtualWorkspaceId;
use crate::model::server::{WindowData, WorkspaceData};
use crate::sys::hotkey::{Hotkey, KeyCode, Modifiers};
use crate::sys::screen::SpaceId;
use crate::ui::common::compute_window_layout_metrics;

const CELL_WIDTH: f64 = 20.0;
const CELL_HEIGHT: f64 = 15.0;
const CELL_SPACING: f64 = 4.0;
const CORNER_RADIUS: f64 = 3.0;
const BORDER_WIDTH: f64 = 1.0;
const CONTENT_INSET: f64 = 2.0;
const FONT_SIZE: f64 = 12.0;

#[derive(Debug, Clone, Copy)]
pub enum MenuAction {
    SetLayout(LayoutMode),
    ToggleSpaceActivated,
    NextWorkspace,
    PrevWorkspace,
    SwitchToWorkspace(usize),
    OpenGitHub,
    OpenDocumentation,
    OpenMatrix,
    OpenConfig,
    ReloadConfig,
    QuitRift,
}

pub struct MenuIcon {
    status_item: Retained<NSStatusItem>,
    view: Retained<MenuIconView>,
    menu: Retained<NSMenu>,
    menu_handler: Retained<MenuActionHandler>,
    mtm: MainThreadMarker,
    prev_width: f64,
}

impl MenuIcon {
    pub fn new(mtm: MainThreadMarker, action_tx: UnboundedSender<MenuAction>) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        let view = MenuIconView::new(mtm);
        let menu_handler = MenuActionHandler::new(mtm, action_tx);
        let menu = build_status_menu(
            mtm,
            &menu_handler,
            None,
            SpaceId::new(0),
            true,
            &[],
            &MenuShortcuts::default(),
        );
        status_item.setMenu(Some(&menu));
        if let Some(btn) = status_item.button(mtm) {
            btn.addSubview(&*view);
            view.setFrameSize(NSSize::new(0.0, 0.0));
            status_item.setVisible(true);
        }

        Self {
            status_item,
            view,
            menu,
            menu_handler,
            mtm,
            prev_width: 0.0,
        }
    }

    pub fn update(
        &mut self,
        active_space: SpaceId,
        active_space_is_activated: bool,
        workspaces: &[WorkspaceData],
        _active_workspace: Option<VirtualWorkspaceId>,
        _windows: &[WindowData],
        settings: &MenuBarSettings,
        hotkeys: &[(Hotkey, WmCommand)],
    ) {
        let ordered_workspaces =
            order_workspaces_for_menu_bar(workspaces, &settings.workspace_order);
        let active_layout = ordered_workspaces
            .iter()
            .find(|w| w.is_active)
            .and_then(|w| parse_layout_mode(&w.layout_mode));
        let shortcuts = MenuShortcuts::from_hotkeys(hotkeys);
        let menu = build_status_menu(
            self.mtm,
            &self.menu_handler,
            active_layout,
            active_space,
            active_space_is_activated,
            &ordered_workspaces,
            &shortcuts,
        );
        self.status_item.setMenu(Some(&menu));
        self.menu = menu;

        let mode = settings.mode;
        let style = settings.display_style;
        let label_for = |workspace: &WorkspaceData| match settings.active_label {
            ActiveWorkspaceLabel::Index => format!("{}", workspace.index + 1),
            ActiveWorkspaceLabel::Name => {
                if workspace.name.is_empty() {
                    format!("{}", workspace.index + 1)
                } else {
                    workspace.name.clone()
                }
            }
        };

        let render_inputs = match (mode, style) {
            (MenuBarDisplayMode::All, WorkspaceDisplayStyle::Layout) => {
                let filtered = if settings.show_empty {
                    ordered_workspaces.clone()
                } else {
                    ordered_workspaces
                        .iter()
                        .cloned()
                        .filter(|w| w.window_count > 0 || w.is_active)
                        .collect::<Vec<_>>()
                };
                filtered
                    .into_iter()
                    .map(|ws| WorkspaceRenderInput {
                        activity_state: workspace_activity_state(&ws, &settings.activity_rules),
                        workspace: ws,
                        label: String::new(),
                        show_windows: true,
                    })
                    .collect()
            }
            (MenuBarDisplayMode::All, WorkspaceDisplayStyle::Label) => ordered_workspaces
                .iter()
                .cloned()
                .filter(|w| settings.show_empty || w.window_count > 0 || w.is_active)
                .map(|ws| {
                    let mut clone = ws.clone();
                    clone.windows.clear();
                    clone.window_count = 0;
                    WorkspaceRenderInput {
                        activity_state: workspace_activity_state(&ws, &settings.activity_rules),
                        workspace: clone,
                        label: label_for(&ws),
                        show_windows: false,
                    }
                })
                .collect(),
            (MenuBarDisplayMode::Active, WorkspaceDisplayStyle::Layout) => ordered_workspaces
                .iter()
                .cloned()
                .find(|w| w.is_active)
                .map(|ws| {
                    vec![WorkspaceRenderInput {
                        activity_state: workspace_activity_state(&ws, &settings.activity_rules),
                        workspace: ws,
                        label: String::new(),
                        show_windows: true,
                    }]
                })
                .unwrap_or_default(),
            (MenuBarDisplayMode::Active, WorkspaceDisplayStyle::Label) => ordered_workspaces
                .iter()
                .cloned()
                .find(|w| w.is_active)
                .map(|ws| {
                    let mut clone = ws.clone();
                    clone.windows.clear();
                    clone.window_count = 0;
                    vec![WorkspaceRenderInput {
                        activity_state: workspace_activity_state(&ws, &settings.activity_rules),
                        workspace: clone,
                        label: label_for(&ws),
                        show_windows: false,
                    }]
                })
                .unwrap_or_default(),
        };

        if render_inputs.is_empty() {
            self.status_item.setVisible(false);
            self.prev_width = 0.0;
            return;
        }

        let layout = {
            let view_ivars = self.view.ivars();
            let active_attrs = view_ivars.active_text_attrs.as_ref();
            let inactive_attrs = view_ivars.inactive_text_attrs.as_ref();
            build_layout(
                &render_inputs,
                active_attrs,
                inactive_attrs,
                &settings.activity_styles,
            )
        };
        if layout.workspaces.is_empty() {
            self.status_item.setVisible(false);
            self.prev_width = 0.0;
            return;
        }

        let size = NSSize::new(layout.total_width, layout.total_height);
        self.view.set_layout(layout);

        self.status_item.setLength(size.width);
        self.status_item.setVisible(true);

        if let Some(btn) = self.status_item.button(self.mtm) {
            if self.prev_width != size.width {
                self.prev_width = size.width;
                btn.setNeedsLayout(true);
            }

            self.view.setFrameSize(size);
            let btn_bounds = btn.bounds();
            let x = (btn_bounds.size.width - size.width) / 2.0;
            let y = (btn_bounds.size.height - size.height) / 2.0;
            self.view.setFrameOrigin(CGPoint::new(x, y));
        }

        self.view.setNeedsDisplay(true);
    }
}

impl Drop for MenuIcon {
    fn drop(&mut self) {
        debug!("Removing menu bar icon");

        let status_bar = NSStatusBar::systemStatusBar();
        status_bar.removeStatusItem(&self.status_item);
    }
}

#[derive(Default)]
struct MenuIconLayout {
    total_width: f64,
    total_height: f64,
    workspaces: Vec<WorkspaceRenderData>,
}

struct WorkspaceRenderData {
    bg_rect: CGRect,
    fill_alpha: f64,
    border_alpha: f64,
    border_width: f64,
    window_alpha: f64,
    windows: Vec<CGRect>,
    label_line: Option<CachedTextLine>,
    show_windows: bool,
}

struct WorkspaceRenderInput {
    workspace: WorkspaceData,
    activity_state: MenuBarWorkspaceActivityState,
    label: String,
    show_windows: bool,
}

struct CachedTextLine {
    line: CFRetained<CTLine>,
    width: f64,
    ascent: f64,
    descent: f64,
}

struct MenuIconViewIvars {
    layout: RefCell<MenuIconLayout>,
    active_text_attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
    inactive_text_attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>>,
}

fn as_any_object<T: Message>(obj: &T) -> &AnyObject {
    unsafe { &*(obj as *const T as *const AnyObject) }
}

fn parse_layout_mode(layout_mode: &str) -> Option<LayoutMode> {
    match layout_mode {
        "traditional" => Some(LayoutMode::Traditional),
        "bsp" => Some(LayoutMode::Bsp),
        "stack" => Some(LayoutMode::Stack),
        "master_stack" => Some(LayoutMode::MasterStack),
        "scrolling" => Some(LayoutMode::Scrolling),
        _ => None,
    }
}

fn order_workspaces_for_menu_bar(
    workspaces: &[WorkspaceData],
    workspace_order: &[WorkspaceSelector],
) -> Vec<WorkspaceData> {
    if workspace_order.is_empty() {
        return workspaces.to_vec();
    }

    let mut ordered = Vec::with_capacity(workspaces.len());
    let mut seen_indexes = std::collections::HashSet::with_capacity(workspaces.len());

    for selector in workspace_order {
        let matched = match selector {
            WorkspaceSelector::Index(index) => workspaces.iter().find(|ws| ws.index == *index),
            WorkspaceSelector::Name(name) => workspaces.iter().find(|ws| ws.name == *name),
        };

        if let Some(workspace) = matched {
            if seen_indexes.insert(workspace.index) {
                ordered.push(workspace.clone());
            }
        }
    }

    for workspace in workspaces {
        if seen_indexes.insert(workspace.index) {
            ordered.push(workspace.clone());
        }
    }

    ordered
}

fn workspace_selector_matches(workspace: &WorkspaceData, selector: &WorkspaceSelector) -> bool {
    match selector {
        WorkspaceSelector::Index(index) => workspace.index == *index,
        WorkspaceSelector::Name(name) => workspace.name == *name,
    }
}

fn rule_matches_window(rule: &MenuBarWorkspaceActivityRule, window: &WindowData) -> bool {
    if let Some(app_id) = &rule.app_id {
        let Some(bundle_id) = window.info.bundle_id.as_deref() else {
            return false;
        };
        if !app_id.eq_ignore_ascii_case(bundle_id) {
            return false;
        }
    }

    if let Some(app_name) = &rule.app_name {
        let Some(window_app_name) = window.app_name.as_deref() else {
            return false;
        };
        let app_name_l = app_name.to_lowercase();
        let window_app_name_l = window_app_name.to_lowercase();
        if !(window_app_name_l.contains(&app_name_l) || app_name_l.contains(&window_app_name_l)) {
            return false;
        }
    }

    if let Some(title_regex) = &rule.title_regex {
        if title_regex.is_empty() {
            return false;
        }
        let Ok(regex) = RegexBuilder::new(title_regex).case_insensitive(true).build() else {
            return false;
        };
        if !regex.is_match(&window.info.title) {
            return false;
        }
    }

    if let Some(title_substring) = &rule.title_substring {
        if title_substring.is_empty() {
            return false;
        }
        let title_l = window.info.title.to_lowercase();
        let title_substring_l = title_substring.to_lowercase();
        if !title_l.contains(&title_substring_l) {
            return false;
        }
    }

    true
}

fn workspace_activity_state(
    workspace: &WorkspaceData,
    rules: &[MenuBarWorkspaceActivityRule],
) -> MenuBarWorkspaceActivityState {
    let mut best_state = MenuBarWorkspaceActivityState::Idle;
    let mut best_priority = i32::MIN;

    for rule in rules {
        if let Some(selector) = &rule.workspace
            && !workspace_selector_matches(workspace, selector)
        {
            continue;
        }

        let has_window_matchers = rule.app_id.is_some()
            || rule.app_name.is_some()
            || rule.title_regex.is_some()
            || rule.title_substring.is_some();

        let matched = if has_window_matchers {
            workspace.windows.iter().any(|w| rule_matches_window(rule, w))
        } else {
            true
        };

        if matched && rule.priority > best_priority {
            best_priority = rule.priority;
            best_state = rule.state;
        }
    }

    best_state
}

fn layout_title(mode: LayoutMode) -> &'static str {
    match mode {
        LayoutMode::Traditional => "Traditional",
        LayoutMode::Bsp => "BSP",
        LayoutMode::Stack => "Stack",
        LayoutMode::MasterStack => "Master Stack",
        LayoutMode::Scrolling => "Scrolling",
    }
}

fn make_menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<objc2::runtime::Sel>,
    target: Option<&MenuActionHandler>,
    checked: Option<bool>,
    key_equivalent: Option<&Hotkey>,
    tag: Option<isize>,
) -> Retained<NSMenuItem> {
    let ns_title = NSString::from_str(title);
    let key_equivalent_empty = NSString::from_str("");
    let item: Retained<NSMenuItem> = unsafe {
        msg_send![NSMenuItem::alloc(mtm), initWithTitle: &*ns_title, action: action, keyEquivalent: &*key_equivalent_empty]
    };
    if let Some(target) = target {
        unsafe {
            item.setTarget(Some(target));
        }
    }
    if let Some(checked) = checked {
        item.setState(if checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }

    if let Some((key, modifiers)) = key_equivalent.and_then(menu_hotkey_to_key_equivalent) {
        let key = NSString::from_str(key);
        item.setKeyEquivalent(&key);
        item.setKeyEquivalentModifierMask(modifiers);
    }
    if let Some(tag) = tag {
        item.setTag(tag);
    }

    item
}

fn add_separator(menu: &NSMenu) {
    let separator: Retained<NSMenuItem> = unsafe { msg_send![NSMenuItem::class(), separatorItem] };
    menu.addItem(&separator);
}

fn build_status_menu(
    mtm: MainThreadMarker,
    handler: &MenuActionHandler,
    active_layout: Option<LayoutMode>,
    _active_space: SpaceId,
    active_space_is_activated: bool,
    workspaces: &[WorkspaceData],
    shortcuts: &MenuShortcuts,
) -> Retained<NSMenu> {
    let title = NSString::from_str("Rift");
    let menu: Retained<NSMenu> = unsafe { msg_send![NSMenu::alloc(mtm), initWithTitle: &*title] };

    let layout_item = make_menu_item(mtm, "Layout", None, None, None, None, None);
    let layout_submenu_title = NSString::from_str("Layout");
    let layout_submenu: Retained<NSMenu> =
        unsafe { msg_send![NSMenu::alloc(mtm), initWithTitle: &*layout_submenu_title] };

    for mode in [
        LayoutMode::Traditional,
        LayoutMode::Bsp,
        LayoutMode::Stack,
        LayoutMode::MasterStack,
        LayoutMode::Scrolling,
    ] {
        let action = match mode {
            LayoutMode::Traditional => sel!(onSetLayoutTraditional:),
            LayoutMode::Bsp => sel!(onSetLayoutBsp:),
            LayoutMode::Stack => sel!(onSetLayoutStack:),
            LayoutMode::MasterStack => sel!(onSetLayoutMasterStack:),
            LayoutMode::Scrolling => sel!(onSetLayoutScrolling:),
        };
        let item = make_menu_item(
            mtm,
            layout_title(mode),
            Some(action),
            Some(handler),
            Some(active_layout == Some(mode)),
            None,
            None,
        );
        layout_submenu.addItem(&item);
    }
    layout_item.setSubmenu(Some(&layout_submenu));
    menu.addItem(&layout_item);

    let workspace_item = make_menu_item(mtm, "Workspaces", None, None, None, None, None);
    let ws_submenu_title = NSString::from_str("Workspace");
    let ws_submenu: Retained<NSMenu> =
        unsafe { msg_send![NSMenu::alloc(mtm), initWithTitle: &*ws_submenu_title] };

    ws_submenu.addItem(&make_menu_item(
        mtm,
        "Next Workspace",
        Some(sel!(onNextWorkspace:)),
        Some(handler),
        None,
        shortcuts.next_workspace.as_ref(),
        None,
    ));
    ws_submenu.addItem(&make_menu_item(
        mtm,
        "Previous Workspace",
        Some(sel!(onPrevWorkspace:)),
        Some(handler),
        None,
        shortcuts.prev_workspace.as_ref(),
        None,
    ));
    add_separator(&ws_submenu);

    for ws in workspaces {
        let ws_label = if ws.name.is_empty() {
            format!("Workspace {}", ws.index + 1)
        } else {
            format!("{} ({})", ws.name, ws.index + 1)
        };
        let ws_shortcut = shortcuts
            .switch_workspace_by_index
            .get(&ws.index)
            .or_else(|| shortcuts.switch_workspace_by_name.get(&ws.name));
        let ws_item = make_menu_item(
            mtm,
            &ws_label,
            Some(sel!(onSwitchWorkspace:)),
            Some(handler),
            Some(ws.is_active),
            ws_shortcut,
            Some(ws.index as isize),
        );
        ws_submenu.addItem(&ws_item);
    }
    if workspaces.is_empty() {
        workspace_item.setEnabled(false);
    } else {
        workspace_item.setSubmenu(Some(&ws_submenu));
    }
    menu.addItem(&workspace_item);

    menu.addItem(&make_menu_item(
        mtm,
        "Enable Tiling",
        Some(sel!(onToggleSpaceActivation:)),
        Some(handler),
        Some(active_space_is_activated),
        shortcuts.toggle_space_activation.as_ref(),
        None,
    ));

    add_separator(&menu);
    menu.addItem(&make_menu_item(
        mtm,
        "Settings…",
        Some(sel!(onOpenConfig:)),
        Some(handler),
        None,
        None,
        None,
    ));
    menu.addItem(&make_menu_item(
        mtm,
        "Reload Config",
        Some(sel!(onReloadConfig:)),
        Some(handler),
        None,
        None,
        None,
    ));

    let help_item = make_menu_item(mtm, "Help / Documentation", None, None, None, None, None);
    let help_submenu_title = NSString::from_str("Help / Documentation");
    let help_submenu: Retained<NSMenu> =
        unsafe { msg_send![NSMenu::alloc(mtm), initWithTitle: &*help_submenu_title] };
    help_submenu.addItem(&make_menu_item(
        mtm,
        "Documentation",
        Some(sel!(onOpenDocumentation:)),
        Some(handler),
        None,
        None,
        None,
    ));
    help_submenu.addItem(&make_menu_item(
        mtm,
        "GitHub",
        Some(sel!(onOpenGitHub:)),
        Some(handler),
        None,
        None,
        None,
    ));
    help_submenu.addItem(&make_menu_item(
        mtm,
        "Matrix",
        Some(sel!(onOpenMatrix:)),
        Some(handler),
        None,
        None,
        None,
    ));
    help_item.setSubmenu(Some(&help_submenu));
    menu.addItem(&help_item);

    add_separator(&menu);
    menu.addItem(&make_menu_item(
        mtm,
        "Quit Rift",
        Some(sel!(onQuitRift:)),
        Some(handler),
        None,
        shortcuts.quit_rift.as_ref(),
        None,
    ));

    menu
}

#[derive(Default)]
struct MenuShortcuts {
    toggle_space_activation: Option<Hotkey>,
    next_workspace: Option<Hotkey>,
    prev_workspace: Option<Hotkey>,
    quit_rift: Option<Hotkey>,
    switch_workspace_by_index: HashMap<usize, Hotkey>,
    switch_workspace_by_name: HashMap<String, Hotkey>,
}

impl MenuShortcuts {
    fn from_hotkeys(hotkeys: &[(Hotkey, WmCommand)]) -> Self {
        let mut out = Self::default();

        for (hotkey, command) in hotkeys {
            match command {
                WmCommand::Wm(WmCmd::ToggleSpaceActivated) => {
                    out.toggle_space_activation.get_or_insert_with(|| hotkey.clone());
                }
                WmCommand::Wm(WmCmd::NextWorkspace) => {
                    out.next_workspace.get_or_insert_with(|| hotkey.clone());
                }
                WmCommand::Wm(WmCmd::PrevWorkspace) => {
                    out.prev_workspace.get_or_insert_with(|| hotkey.clone());
                }
                WmCommand::Wm(WmCmd::SwitchToWorkspace(WorkspaceSelector::Index(i))) => {
                    out.switch_workspace_by_index.entry(*i).or_insert_with(|| hotkey.clone());
                }
                WmCommand::Wm(WmCmd::SwitchToWorkspace(WorkspaceSelector::Name(name))) => {
                    out.switch_workspace_by_name
                        .entry(name.clone())
                        .or_insert_with(|| hotkey.clone());
                }
                WmCommand::ReactorCommand(ReactorTopCommand::Reactor(
                    ReactorCommand::ToggleSpaceActivated,
                )) => {
                    out.toggle_space_activation.get_or_insert_with(|| hotkey.clone());
                }
                WmCommand::ReactorCommand(ReactorTopCommand::Layout(
                    LayoutCommand::NextWorkspace(_),
                )) => {
                    out.next_workspace.get_or_insert_with(|| hotkey.clone());
                }
                WmCommand::ReactorCommand(ReactorTopCommand::Layout(
                    LayoutCommand::PrevWorkspace(_),
                )) => {
                    out.prev_workspace.get_or_insert_with(|| hotkey.clone());
                }
                WmCommand::ReactorCommand(ReactorTopCommand::Layout(
                    LayoutCommand::SwitchToWorkspace(i),
                )) => {
                    out.switch_workspace_by_index.entry(*i).or_insert_with(|| hotkey.clone());
                }
                WmCommand::ReactorCommand(ReactorTopCommand::Reactor(
                    ReactorCommand::SaveAndExit,
                )) => {
                    out.quit_rift.get_or_insert_with(|| hotkey.clone());
                }
                _ => {}
            }
        }

        out
    }
}

fn menu_hotkey_to_key_equivalent(hotkey: &Hotkey) -> Option<(&'static str, NSEventModifierFlags)> {
    let key = match hotkey.key_code {
        KeyCode::KeyA => "a",
        KeyCode::KeyB => "b",
        KeyCode::KeyC => "c",
        KeyCode::KeyD => "d",
        KeyCode::KeyE => "e",
        KeyCode::KeyF => "f",
        KeyCode::KeyG => "g",
        KeyCode::KeyH => "h",
        KeyCode::KeyI => "i",
        KeyCode::KeyJ => "j",
        KeyCode::KeyK => "k",
        KeyCode::KeyL => "l",
        KeyCode::KeyM => "m",
        KeyCode::KeyN => "n",
        KeyCode::KeyO => "o",
        KeyCode::KeyP => "p",
        KeyCode::KeyQ => "q",
        KeyCode::KeyR => "r",
        KeyCode::KeyS => "s",
        KeyCode::KeyT => "t",
        KeyCode::KeyU => "u",
        KeyCode::KeyV => "v",
        KeyCode::KeyW => "w",
        KeyCode::KeyX => "x",
        KeyCode::KeyY => "y",
        KeyCode::KeyZ => "z",
        KeyCode::Digit0 => "0",
        KeyCode::Digit1 => "1",
        KeyCode::Digit2 => "2",
        KeyCode::Digit3 => "3",
        KeyCode::Digit4 => "4",
        KeyCode::Digit5 => "5",
        KeyCode::Digit6 => "6",
        KeyCode::Digit7 => "7",
        KeyCode::Digit8 => "8",
        KeyCode::Digit9 => "9",
        KeyCode::Minus => "-",
        KeyCode::Equal => "=",
        KeyCode::BracketLeft => "[",
        KeyCode::BracketRight => "]",
        KeyCode::Semicolon => ";",
        KeyCode::Quote => "'",
        KeyCode::Backquote => "`",
        KeyCode::Backslash => "\\",
        KeyCode::Comma => ",",
        KeyCode::Period => ".",
        KeyCode::Slash => "/",
        _ => return None,
    };

    let mut flags = NSEventModifierFlags::empty();
    if hotkey.modifiers.intersects(Modifiers::META) {
        flags.insert(NSEventModifierFlags::Command);
    }
    if hotkey.modifiers.intersects(Modifiers::CONTROL) {
        flags.insert(NSEventModifierFlags::Control);
    }
    if hotkey.modifiers.intersects(Modifiers::ALT) {
        flags.insert(NSEventModifierFlags::Option);
    }
    if hotkey.modifiers.intersects(Modifiers::SHIFT) {
        flags.insert(NSEventModifierFlags::Shift);
    }

    Some((key, flags))
}

struct MenuActionHandlerIvars {
    action_tx: UnboundedSender<MenuAction>,
}

impl MenuActionHandler {
    fn new(mtm: MainThreadMarker, action_tx: UnboundedSender<MenuAction>) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(MenuActionHandlerIvars { action_tx });
        unsafe { msg_send![super(this), init] }
    }

    fn emit(&self, action: MenuAction) { let _ = self.ivars().action_tx.send(action); }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RiftMenuBarActionHandler"]
    #[ivars = MenuActionHandlerIvars]
    struct MenuActionHandler;

    impl MenuActionHandler {
        #[unsafe(method(onSetLayoutTraditional:))]
        fn on_set_layout_traditional(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::SetLayout(LayoutMode::Traditional));
        }

        #[unsafe(method(onSetLayoutBsp:))]
        fn on_set_layout_bsp(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::SetLayout(LayoutMode::Bsp));
        }

        #[unsafe(method(onSetLayoutStack:))]
        fn on_set_layout_stack(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::SetLayout(LayoutMode::Stack));
        }

        #[unsafe(method(onSetLayoutMasterStack:))]
        fn on_set_layout_master_stack(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::SetLayout(LayoutMode::MasterStack));
        }

        #[unsafe(method(onSetLayoutScrolling:))]
        fn on_set_layout_scrolling(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::SetLayout(LayoutMode::Scrolling));
        }

        #[unsafe(method(onToggleSpaceActivation:))]
        fn on_toggle_space_activation(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::ToggleSpaceActivated);
        }

        #[unsafe(method(onNextWorkspace:))]
        fn on_next_workspace(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::NextWorkspace);
        }

        #[unsafe(method(onPrevWorkspace:))]
        fn on_prev_workspace(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::PrevWorkspace);
        }

        #[unsafe(method(onSwitchWorkspace:))]
        fn on_switch_workspace(&self, sender: Option<&NSMenuItem>) {
            if let Some(sender) = sender {
                let tag = sender.tag();
                if tag >= 0 {
                    self.emit(MenuAction::SwitchToWorkspace(tag as usize));
                }
            }
        }

        #[unsafe(method(onOpenConfig:))]
        fn on_open_config(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::OpenConfig);
        }

        #[unsafe(method(onOpenDocumentation:))]
        fn on_open_documentation(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::OpenDocumentation);
        }

        #[unsafe(method(onOpenGitHub:))]
        fn on_open_github(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::OpenGitHub);
        }

        #[unsafe(method(onOpenMatrix:))]
        fn on_open_matrix(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::OpenMatrix);
        }

        #[unsafe(method(onReloadConfig:))]
        fn on_reload_config(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::ReloadConfig);
        }

        #[unsafe(method(onQuitRift:))]
        fn on_quit_rift(&self, _sender: Option<&AnyObject>) {
            self.emit(MenuAction::QuitRift);
        }
    }
);

fn build_text_attrs(
    font: &NSFont,
    color: &NSColor,
) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let dict = NSMutableDictionary::<NSAttributedStringKey, AnyObject>::new();
    unsafe {
        dict.setObject_forKeyedSubscript(
            Some(as_any_object(font)),
            ProtocolObject::from_ref(NSFontAttributeName),
        );
        dict.setObject_forKeyedSubscript(
            Some(as_any_object(color)),
            ProtocolObject::from_ref(NSForegroundColorAttributeName),
        );
    }
    unsafe { Retained::cast_unchecked(dict) }
}

fn build_cached_text_line(
    label: &str,
    attrs: &NSDictionary<NSAttributedStringKey, AnyObject>,
) -> Option<CachedTextLine> {
    if label.is_empty() {
        return None;
    }

    let label_ns = NSString::from_str(label);
    let cf_string: &CFString = label_ns.as_ref();
    let cf_dict_ref: &CFDictionary<NSAttributedStringKey, AnyObject> = attrs.as_ref();
    let cf_dict: &CFDictionary = cf_dict_ref.as_opaque();
    let attr_string = unsafe { CFAttributedString::new(None, Some(cf_string), Some(cf_dict)) }?;
    let line: CFRetained<CTLine> = unsafe { CTLine::with_attributed_string(attr_string.as_ref()) };

    let mut ascent: CGFloat = 0.0;
    let mut descent: CGFloat = 0.0;
    let mut leading: CGFloat = 0.0;
    let line_ref: &CTLine = line.as_ref();
    let width = unsafe { line_ref.typographic_bounds(&mut ascent, &mut descent, &mut leading) };

    Some(CachedTextLine {
        line,
        width: width as f64,
        ascent: ascent as f64,
        descent: descent as f64,
    })
}

impl MenuIconView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let font = NSFont::menuBarFontOfSize(FONT_SIZE);
        let active_color = NSColor::blackColor();
        let inactive_color = NSColor::whiteColor();
        let active_attrs = build_text_attrs(font.as_ref(), active_color.as_ref());
        let inactive_attrs = build_text_attrs(font.as_ref(), inactive_color.as_ref());

        let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
        let view = mtm.alloc().set_ivars(MenuIconViewIvars {
            layout: RefCell::new(MenuIconLayout::default()),
            active_text_attrs: active_attrs,
            inactive_text_attrs: inactive_attrs,
        });
        unsafe { msg_send![super(view), initWithFrame: frame] }
    }

    fn set_layout(&self, layout: MenuIconLayout) {
        *self.ivars().layout.borrow_mut() = layout;
        self.setNeedsDisplay(true);
    }
}

fn build_layout(
    inputs: &[WorkspaceRenderInput],
    active_attrs: &NSDictionary<NSAttributedStringKey, AnyObject>,
    inactive_attrs: &NSDictionary<NSAttributedStringKey, AnyObject>,
    activity_settings: &crate::common::config::MenuBarWorkspaceActivityStyles,
) -> MenuIconLayout {
    let count = inputs.len();
    let total_width =
        (CELL_WIDTH * count as f64) + (CELL_SPACING * (count.saturating_sub(1) as f64));
    let total_height = CELL_HEIGHT;

    let mut workspaces = Vec::with_capacity(count);
    for (i, input) in inputs.iter().enumerate() {
        let workspace = &input.workspace;
        let bg_x = i as f64 * (CELL_WIDTH + CELL_SPACING);
        let bg_y = 0.0;
        let bg_rect = CGRect::new(CGPoint::new(bg_x, bg_y), CGSize::new(CELL_WIDTH, CELL_HEIGHT));

        let base_fill_alpha = if input.show_windows {
            if workspace.is_active {
                1.0
            } else if workspace.window_count > 0 {
                0.45
            } else {
                0.0
            }
        } else if workspace.is_active {
            1.0
        } else {
            0.35
        };

        let activity_style = match input.activity_state {
            MenuBarWorkspaceActivityState::Idle => activity_settings.idle.clone(),
            MenuBarWorkspaceActivityState::Running => activity_settings.running.clone(),
            MenuBarWorkspaceActivityState::WaitingInput => activity_settings.waiting_input.clone(),
        };

        let (fill_alpha, border_alpha, border_width, window_alpha) =
            if input.activity_state == MenuBarWorkspaceActivityState::Idle {
                (base_fill_alpha, 1.0, BORDER_WIDTH, 1.0)
            } else {
                (
                    activity_style.fill_alpha,
                    activity_style.border_alpha,
                    activity_style.border_width,
                    activity_style.window_alpha,
                )
            };

        let windows = if input.show_windows && !workspace.windows.is_empty() {
            let layout = compute_window_layout_metrics(
                &workspace.windows,
                bg_rect,
                CONTENT_INSET,
                1.0,
                None,
            );
            if let Some(layout) = layout {
                const MIN_TILE_SIZE: f64 = 2.0;
                const WIN_GAP: f64 = 0.75;
                let mut rects = Vec::with_capacity(workspace.windows.len());
                for window in workspace.windows.iter().rev() {
                    let rect = layout.rect_for(window, MIN_TILE_SIZE, WIN_GAP);
                    rects.push(rect);
                }
                rects
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let label_line = if !input.label.is_empty() {
            let attrs = if fill_alpha > 0.0 {
                active_attrs
            } else {
                inactive_attrs
            };
            build_cached_text_line(&input.label, attrs)
        } else {
            None
        };

        workspaces.push(WorkspaceRenderData {
            bg_rect,
            fill_alpha,
            border_alpha,
            border_width,
            window_alpha,
            windows,
            label_line,
            show_windows: input.show_windows,
        });
    }

    MenuIconLayout {
        total_width,
        total_height,
        workspaces,
    }
}

fn add_rounded_rect(ctx: &CGContext, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let ctx = Some(ctx);
    let r = r.min(w / 2.0).min(h / 2.0);
    CGContext::begin_path(ctx);
    CGContext::move_to_point(ctx, x + r, y + h);
    CGContext::add_line_to_point(ctx, x + w - r, y + h);
    CGContext::add_arc_to_point(ctx, x + w, y + h, x + w, y + h - r, r);
    CGContext::add_line_to_point(ctx, x + w, y + r);
    CGContext::add_arc_to_point(ctx, x + w, y, x + w - r, y, r);
    CGContext::add_line_to_point(ctx, x + r, y);
    CGContext::add_arc_to_point(ctx, x, y, x, y + r, r);
    CGContext::add_line_to_point(ctx, x, y + h - r);
    CGContext::add_arc_to_point(ctx, x, y + h, x + r, y + h, r);
    CGContext::close_path(ctx);
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RiftMenuBarIconView"]
    #[ivars = MenuIconViewIvars]
    struct MenuIconView;

    impl MenuIconView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let layout = self.ivars().layout.borrow();
            let bounds = self.bounds();

            if let Some(context) = NSGraphicsContext::currentContext() {
                let cg_context = context.CGContext();
                let cg = cg_context.as_ref();
                CGContext::save_g_state(Some(cg));
                CGContext::clear_rect(Some(cg), bounds);

                let y_offset = (bounds.size.height - layout.total_height) / 2.0;

                for workspace in layout.workspaces.iter() {
                    let rect = workspace.bg_rect;
                    let bg_y = rect.origin.y + y_offset;
                    add_rounded_rect(
                        cg,
                        rect.origin.x,
                        bg_y,
                        rect.size.width,
                        rect.size.height,
                        CORNER_RADIUS,
                    );

                    if workspace.fill_alpha > 0.0 {
                        CGContext::set_rgb_fill_color(
                            Some(cg),
                            1.0,
                            1.0,
                            1.0,
                            workspace.fill_alpha,
                        );
                        CGContext::fill_path(Some(cg));
                    }

                    add_rounded_rect(
                        cg,
                        rect.origin.x,
                        bg_y,
                        rect.size.width,
                        rect.size.height,
                        CORNER_RADIUS,
                    );
                    CGContext::set_rgb_stroke_color(
                        Some(cg),
                        1.0,
                        1.0,
                        1.0,
                        workspace.border_alpha,
                    );
                    CGContext::set_line_width(Some(cg), workspace.border_width);
                    CGContext::stroke_path(Some(cg));

                    if workspace.show_windows {
                        for window in workspace.windows.iter() {
                            add_rounded_rect(
                                cg,
                                window.origin.x,
                                window.origin.y + y_offset,
                                window.size.width,
                                window.size.height,
                                1.5,
                            );
                            CGContext::set_rgb_fill_color(
                                Some(cg),
                                1.0,
                                1.0,
                                1.0,
                                workspace.window_alpha,
                            );
                            CGContext::fill_path(Some(cg));

                            CGContext::save_g_state(Some(cg));
                            CGContext::set_blend_mode(Some(cg), CGBlendMode::DestinationOut);
                            CGContext::set_rgb_stroke_color(Some(cg), 1.0, 1.0, 1.0, 1.0);
                            CGContext::set_line_width(Some(cg), 1.5);
                            add_rounded_rect(
                                cg,
                                window.origin.x,
                                window.origin.y,
                                window.size.width,
                                window.size.height,
                                1.5,
                            );
                            CGContext::stroke_path(Some(cg));
                            CGContext::restore_g_state(Some(cg));
                        }
                    }

                    if let Some(label_line) = &workspace.label_line {
                        let text_width = label_line.width;
                        let text_center_y = bg_y + rect.size.height / 2.0;
                        let baseline_y = text_center_y - (label_line.ascent - label_line.descent) / 2.0;
                        let text_x = rect.origin.x + (rect.size.width - text_width) / 2.0;

                        CGContext::save_g_state(Some(cg));
                        if workspace.fill_alpha > 0.0 {
                            CGContext::set_rgb_fill_color(Some(cg), 0.0, 0.0, 0.0, 1.0);
                        } else {
                            CGContext::set_rgb_fill_color(Some(cg), 1.0, 1.0, 1.0, 1.0);
                        }
                        CGContext::set_text_position(Some(cg), text_x as CGFloat, baseline_y as CGFloat);
                        let line_ref: &CTLine = label_line.line.as_ref();
                        unsafe { line_ref.draw(cg) };
                        CGContext::restore_g_state(Some(cg));
                    }
                }

                CGContext::restore_g_state(Some(cg));
            }
        }
    }
);

#[cfg(test)]
mod tests {
    use super::{order_workspaces_for_menu_bar, workspace_activity_state};
    use crate::actor::app::WindowId;
    use crate::common::config::{
        MenuBarWorkspaceActivityRule, MenuBarWorkspaceActivityState, WorkspaceSelector,
    };
    use crate::model::server::{WindowData, WorkspaceData};
    use crate::sys::app::WindowInfo;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn window(app: &str, bundle: &str, title: &str) -> WindowData {
        WindowData {
            id: WindowId::new(100, 1),
            is_floating: false,
            is_focused: false,
            app_name: Some(app.to_string()),
            info: WindowInfo {
                is_standard: true,
                is_root: true,
                is_minimized: false,
                is_resizable: true,
                min_size: None,
                max_size: None,
                title: title.to_string(),
                frame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(100.0, 100.0)),
                sys_id: None,
                bundle_id: Some(bundle.to_string()),
                path: None,
                ax_role: None,
                ax_subrole: None,
            },
        }
    }

    fn workspace(index: usize, name: &str, windows: Vec<WindowData>) -> WorkspaceData {
        WorkspaceData {
            id: format!("ws-{index}"),
            index,
            name: name.to_string(),
            layout_mode: "bsp".to_string(),
            is_active: index == 0,
            window_count: windows.len(),
            windows,
        }
    }

    #[test]
    fn workspace_activity_state_prefers_higher_priority_rule() {
        let ws = workspace(
            1,
            "Dev",
            vec![window("Ghostty", "com.mitchellh.ghostty", "Claude Code - Waiting for input")],
        );

        let rules = vec![
            MenuBarWorkspaceActivityRule {
                state: MenuBarWorkspaceActivityState::Running,
                workspace: None,
                app_id: Some("com.mitchellh.ghostty".to_string()),
                app_name: None,
                title_regex: None,
                title_substring: None,
                priority: 10,
            },
            MenuBarWorkspaceActivityRule {
                state: MenuBarWorkspaceActivityState::WaitingInput,
                workspace: None,
                app_id: Some("com.mitchellh.ghostty".to_string()),
                app_name: None,
                title_regex: None,
                title_substring: Some("waiting for input".to_string()),
                priority: 50,
            },
        ];

        let state = workspace_activity_state(&ws, &rules);
        assert_eq!(state, MenuBarWorkspaceActivityState::WaitingInput);
    }

    #[test]
    fn workspace_activity_state_respects_workspace_selector() {
        let ws = workspace(
            1,
            "Dev",
            vec![window("Ghostty", "com.mitchellh.ghostty", "Claude Code")],
        );

        let rules = vec![MenuBarWorkspaceActivityRule {
            state: MenuBarWorkspaceActivityState::Running,
            workspace: Some(WorkspaceSelector::Name("Chat".to_string())),
            app_id: Some("com.mitchellh.ghostty".to_string()),
            app_name: None,
            title_regex: None,
            title_substring: None,
            priority: 100,
        }];

        let state = workspace_activity_state(&ws, &rules);
        assert_eq!(state, MenuBarWorkspaceActivityState::Idle);
    }

    #[test]
    fn workspace_order_applies_selected_first_then_remaining_in_natural_order() {
        let workspaces = vec![
            workspace(0, "Main", vec![]),
            workspace(1, "Dev", vec![]),
            workspace(2, "Chat", vec![]),
            workspace(3, "Misc", vec![]),
        ];

        let ordered = order_workspaces_for_menu_bar(
            &workspaces,
            &[WorkspaceSelector::Name("Chat".to_string()), WorkspaceSelector::Index(0)],
        );

        let indexes: Vec<usize> = ordered.iter().map(|ws| ws.index).collect();
        assert_eq!(indexes, vec![2, 0, 1, 3]);
    }
}
