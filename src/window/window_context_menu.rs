use super::*;

pub(super) fn show_context_menu_document(
    hwnd: HWND,
    reference: Option<&str>,
    origin: Option<(usize, String)>,
) {
    let document = match context_menu::resolve_context_menu(reference) {
        Ok(document) => document,
        Err(error) => {
            diagnose::log(format!("context menu load failed: {error}"));
            context_menu::classic_context_menu()
        }
    };
    let language = lock_state()
        .as_ref()
        .map(|state| state.language)
        .unwrap_or_else(localization::detect_system_language);
    let data_context = context_menu_data_context(origin.as_ref());
    let mut actions = Vec::new();
    let mut account_ids = Vec::new();
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        append_account_menu(menu, &mut account_ids);
        append_context_menu_items(
            menu,
            &document.items,
            language,
            &data_context,
            origin.as_ref(),
            &mut actions,
        );
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        if selected >= ACCOUNT_MENU_BASE {
            if let Some(id) = account_ids.get(selected - ACCOUNT_MENU_BASE).cloned() {
                switch_account_from_menu(hwnd, id);
            }
        } else if selected >= 1_000 {
            if let Some(action) = actions.get(selected - 1_000).cloned() {
                execute_context_menu_action(hwnd, action, origin);
            }
        }
    }
}

/// Command ids at and above this value select an account, not a menu action.
const ACCOUNT_MENU_BASE: usize = 5_000;

/// Claude accounts with their usage, checked on the one that VS Code and
/// terminals currently use. Built from live state, so it works with any menu
/// document or theme.
unsafe fn append_account_menu(menu: HMENU, ids: &mut Vec<String>) {
    use super::account_menu_draw::{register, AccountRow, Window};
    let window = |section: &crate::models::UsageSection| {
        section.available.then(|| Window {
            percentage: section.percentage,
            reset: crate::account_switch::format_reset(section.resets_at),
        })
    };
    let rows: Vec<(String, AccountRow, bool)> = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        if !state.providers.contains(ProviderId::Claude) {
            return;
        }
        let profiles = &state.accounts.claude.profiles;
        let live = crate::account_switch::live_profile_index(profiles);
        profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| profile.enabled)
            .map(|(index, profile)| {
                let account = state.data.as_ref().and_then(|data| {
                    data.accounts.iter().find(|account| {
                        account.provider == ProviderId::Claude
                            && account.profile.same_source(profile)
                    })
                });
                let usage = account.and_then(|account| account.usage.as_ref());
                let note = match (usage, account.map(|account| account.error)) {
                    (Some(_), _) => None,
                    (None, Some(Some(_))) => Some("sign-in needed".to_string()),
                    _ => Some("waiting for usage".to_string()),
                };
                let is_live = live == Some(index);
                let can_switch =
                    crate::account_switch::profile_directory(profile).is_some() || is_live;
                (
                    profile.id.clone(),
                    AccountRow {
                        name: profile.name.clone(),
                        session: usage.and_then(|usage| window(&usage.session)),
                        weekly: usage.and_then(|usage| window(&usage.weekly)),
                        note,
                        live: is_live,
                    },
                    can_switch,
                )
            })
            .collect()
    };
    if rows.is_empty() {
        return;
    }
    let header = native_interop::wide_str("Claude accounts \u{2013} click to switch");
    let _ = AppendMenuW(menu, MF_GRAYED, 0, PCWSTR::from_raw(header.as_ptr()));
    let item_data = register(rows.iter().map(|(_, row, _)| row.clone()).collect());
    for ((id, _, can_switch), data) in rows.into_iter().zip(item_data) {
        let mut flags = MF_OWNERDRAW;
        if !can_switch {
            flags |= MF_GRAYED;
        }
        let command = ACCOUNT_MENU_BASE + ids.len();
        let _ = AppendMenuW(menu, flags, command, PCWSTR(data as *const u16));
        ids.push(id);
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
}

/// Swaps the live account off the UI thread (it touches files and the
/// registry), then tells the monitor to reload so usage and checks refresh.
fn switch_account_from_menu(hwnd: HWND, profile_id: String) {
    let window = hwnd.0 as isize;
    std::thread::spawn(move || {
        let mut settings = load_settings();
        let result = settings
            .accounts
            .claude
            .profiles
            .iter()
            .position(|profile| profile.id == profile_id)
            .ok_or_else(|| "Account not found".to_string())
            .and_then(|index| {
                crate::account_switch::swap_live_account(&mut settings.accounts.claude, index)
            });
        match result {
            Ok(message) => {
                diagnose::log(format!("account switched from menu: {message}"));
                save_settings_or_log(&settings, "unable to save accounts after switching");
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(window as *mut _)),
                        WM_APP_SETTINGS_UPDATED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            Err(error) => unsafe {
                let text = native_interop::wide_str(&format!("Could not switch account: {error}"));
                let title = native_interop::wide_str("Usage Monitor");
                let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    None,
                    PCWSTR::from_raw(text.as_ptr()),
                    PCWSTR::from_raw(title.as_ptr()),
                    windows::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            },
        }
    });
}

pub(super) fn context_menu_data_context(origin: Option<&(usize, String)>) -> DataContext {
    let state = lock_state();
    let Some(state) = state.as_ref() else {
        return DataContext::from_usage(None, &Canvas::default());
    };
    let mut runtime = theme_runtime_from_state(state);
    let mut canvas = Canvas::default();
    if let Some(theme) = effective_theme_from_state(state) {
        let surface_index = origin.map_or(0, |(surface_index, _)| *surface_index);
        if theme.surfaces.get(surface_index).is_some() {
            runtime = theme_runtime_for_surface(&theme, surface_index, runtime);
            let (width, height) = theme_engine::resolve_surface_content_size(
                &theme,
                surface_index,
                state.data.as_ref(),
                runtime,
            );
            canvas.width = width;
            canvas.height = height;
        }
    }
    DataContext::from_usage_with_runtime(state.data.as_ref(), &canvas, runtime)
}

unsafe fn append_context_menu_items(
    menu: HMENU,
    items: &[ContextMenuItem],
    language: LanguageId,
    context: &DataContext,
    origin: Option<&(usize, String)>,
    actions: &mut Vec<ContextMenuAction>,
) {
    for item in items {
        if !item.should_render(context) {
            continue;
        }
        match &item.kind {
            ContextMenuItemKind::Separator => {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            ContextMenuItemKind::Text => {
                let label = native_interop::wide_str(&context_menu::rendered_label(
                    language,
                    &item.label,
                    context,
                ));
                let _ = AppendMenuW(menu, MF_GRAYED, 0, PCWSTR::from_raw(label.as_ptr()));
            }
            ContextMenuItemKind::Submenu { items } => {
                let Ok(submenu) = CreatePopupMenu() else {
                    continue;
                };
                append_context_menu_items(submenu, items, language, context, origin, actions);
                let label = native_interop::wide_str(&context_menu::rendered_label(
                    language,
                    &item.label,
                    context,
                ));
                let _ = AppendMenuW(
                    menu,
                    MF_POPUP,
                    submenu.0 as usize,
                    PCWSTR::from_raw(label.as_ptr()),
                );
            }
            ContextMenuItemKind::Action { action } => {
                let id = 1_000 + actions.len();
                let label = native_interop::wide_str(&context_menu::rendered_label(
                    language,
                    &item.label,
                    context,
                ));
                let flags = context_menu_action_flags(action, origin);
                let _ = AppendMenuW(menu, flags, id, PCWSTR::from_raw(label.as_ptr()));
                actions.push(action.clone());
            }
        }
    }
}

pub(super) fn context_menu_action_flags(
    action: &ContextMenuAction,
    origin: Option<&(usize, String)>,
) -> MENU_ITEM_FLAGS {
    let state = lock_state();
    let Some(state) = state.as_ref() else {
        return MENU_ITEM_FLAGS(0);
    };
    let checked = match action {
        ContextMenuAction::SetUpdateFrequency { seconds } => {
            state.poll_interval_ms == seconds.saturating_mul(1_000)
        }
        ContextMenuAction::ToggleProvider { provider } => state.providers.contains(*provider),
        ContextMenuAction::ToggleAntigravityGemini => state.show_antigravity_gemini,
        ContextMenuAction::ToggleAntigravityExternal => state.show_antigravity_external,
        ContextMenuAction::ToggleStartup => is_startup_enabled(),
        ContextMenuAction::ToggleTaskbarLock => state.lock_taskbar,
        ContextMenuAction::ToggleWidget => state
            .active_theme
            .as_ref()
            .and_then(|theme| {
                let effective = theme_engine::apply_mouse_action_overrides(
                    theme,
                    &state.mouse_action_overrides,
                );
                context_menu_widget_origin(&effective).map(|(surface_index, _)| {
                    let runtime = theme_runtime_for_surface(
                        &effective,
                        surface_index,
                        theme_runtime_from_state(state),
                    );
                    theme_engine::surface_should_render(
                        &effective,
                        surface_index,
                        state.data.as_ref(),
                        runtime,
                    )
                })
            })
            .unwrap_or(false),
        ContextMenuAction::SetLanguage { language } => {
            if language.eq_ignore_ascii_case("system") {
                state.language_override.is_none()
            } else {
                state
                    .language_override
                    .is_some_and(|current| current.code().eq_ignore_ascii_case(language))
            }
        }
        ContextMenuAction::ToggleLayerRender { target } => state
            .active_theme
            .as_ref()
            .and_then(|theme| {
                let effective = theme_engine::apply_mouse_action_overrides(
                    theme,
                    &state.mouse_action_overrides,
                );
                effective
                    .surfaces
                    .iter()
                    .position(|surface| surface.id.eq_ignore_ascii_case(target))
                    .map(|surface_index| {
                        let runtime = theme_runtime_for_surface(
                            &effective,
                            surface_index,
                            theme_runtime_from_state(state),
                        );
                        theme_engine::surface_should_render(
                            &effective,
                            surface_index,
                            state.data.as_ref(),
                            runtime,
                        )
                    })
            })
            .unwrap_or(false),
        _ => false,
    };
    let disabled = matches!(
        action,
        ContextMenuAction::CheckForUpdates
            if matches!(state.update_status, UpdateStatus::Checking | UpdateStatus::Applying)
    ) || matches!(
        action,
        ContextMenuAction::LayerActions { .. } | ContextMenuAction::ToggleLayerRender { .. }
    ) && origin.is_none()
        && state.active_theme.is_none();
    match (checked, disabled) {
        (true, true) => MF_CHECKED | MF_GRAYED,
        (true, false) => MF_CHECKED,
        (false, true) => MF_GRAYED,
        (false, false) => MENU_ITEM_FLAGS(0),
    }
}

pub(super) fn context_menu_action_origin(
    origin: Option<(usize, String)>,
) -> Option<(usize, String)> {
    if origin.is_some() {
        return origin;
    }
    lock_state().as_ref().and_then(|state| {
        state
            .active_theme
            .as_ref()
            .and_then(|theme| theme.surfaces.first())
            .map(|surface| (0, surface.id.clone()))
    })
}

pub(super) fn context_menu_widget_origin(theme: &ThemeDocument) -> Option<(usize, String)> {
    theme
        .surfaces
        .iter()
        .enumerate()
        .find(|(_, surface)| surface.placement.nest != SurfaceNest::TrayIcon)
        .map(|(index, surface)| (index, surface.id.clone()))
}

pub(super) fn execute_context_menu_action(
    hwnd: HWND,
    action: ContextMenuAction,
    origin: Option<(usize, String)>,
) {
    let static_command = match &action {
        ContextMenuAction::OpenDashboard => Some(IDM_DASHBOARD),
        ContextMenuAction::Refresh => Some(1),
        ContextMenuAction::SetUpdateFrequency { seconds } => match *seconds {
            POLL_1_MIN_SECONDS => Some(IDM_FREQ_1MIN),
            POLL_5_MIN_SECONDS => Some(IDM_FREQ_5MIN),
            POLL_15_MIN_SECONDS => Some(IDM_FREQ_15MIN),
            POLL_1_HOUR_SECONDS => Some(IDM_FREQ_1HOUR),
            _ => None,
        },
        ContextMenuAction::ToggleProvider { provider } => {
            Some(provider.descriptor().native_menu_command_id)
        }
        ContextMenuAction::ToggleAntigravityGemini => Some(IDM_PROVIDER_ANTIGRAVITY_GEMINI),
        ContextMenuAction::ToggleAntigravityExternal => Some(IDM_PROVIDER_ANTIGRAVITY_EXTERNAL),
        ContextMenuAction::ToggleStartup => Some(IDM_START_WITH_WINDOWS),
        ContextMenuAction::ToggleTaskbarLock => Some(IDM_LOCK_TASKBAR),
        ContextMenuAction::SetLanguage { language } => {
            if language.eq_ignore_ascii_case("system") {
                Some(IDM_LANG_SYSTEM)
            } else {
                LanguageId::from_code(language).map(language_menu_command_id)
            }
        }
        ContextMenuAction::CheckForUpdates => Some(IDM_VERSION_ACTION),
        ContextMenuAction::Exit => Some(2),
        ContextMenuAction::ToggleWidget
        | ContextMenuAction::LegacyResetPosition
        | ContextMenuAction::ToggleLayerRender { .. }
        | ContextMenuAction::LayerActions { .. }
        | ContextMenuAction::OpenUrl { .. } => None,
    };
    if let Some(command) = static_command {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(command as usize), LPARAM(0));
        }
        return;
    }
    match action {
        ContextMenuAction::ToggleWidget => {
            let target = lock_state()
                .as_ref()
                .and_then(|state| state.active_theme.as_ref())
                .and_then(context_menu_widget_origin);
            if let Some((surface_index, root_id)) = target {
                let _ =
                    execute_mouse_action_source(surface_index, &root_id, "toggle(self, render)");
            }
        }
        ContextMenuAction::ToggleLayerRender { target } => {
            let Some((surface_index, self_id)) = context_menu_action_origin(origin) else {
                return;
            };
            let target = target.replace('\\', "\\\\").replace('"', "\\\"");
            let source = format!("toggle(\"{target}\", render)");
            let _ = execute_mouse_action_source(surface_index, &self_id, &source);
        }
        ContextMenuAction::LayerActions { actions } => {
            let Some((surface_index, self_id)) = context_menu_action_origin(origin) else {
                return;
            };
            let _ = execute_mouse_action_source(surface_index, &self_id, &actions);
        }
        ContextMenuAction::OpenUrl { url } => {
            open_web_url(hwnd, &url, "context menu URL could not be opened")
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetSubMenu};

    #[test]
    fn conditional_native_rows_and_subtrees_preserve_action_ids_on_each_open() {
        let mut conditional = vec![
            ContextMenuItem::text("usage", "Weekly usage"),
            ContextMenuItem::separator("divider"),
            ContextMenuItem::action("refresh", "Refresh", ContextMenuAction::Refresh),
            ContextMenuItem::submenu(
                "provider",
                "Provider",
                vec![ContextMenuItem::action(
                    "nested",
                    "Exit",
                    ContextMenuAction::Exit,
                )],
            ),
        ];
        for item in &mut conditional {
            item.render = theme_engine::Expression("providers.codex.enabled".into());
        }
        // This row stays hidden even when its parent becomes visible.
        if let ContextMenuItemKind::Submenu { items } = &mut conditional[3].kind {
            let mut hidden = ContextMenuItem::text("hidden", "Hidden child");
            hidden.render = theme_engine::Expression("0".into());
            items.insert(0, hidden);
        }
        conditional.push(ContextMenuItem::action(
            "exit",
            "Exit",
            ContextMenuAction::Exit,
        ));
        let mut context = DataContext::from_usage(None, &Canvas::default());
        for enabled in [false, true, false] {
            context.insert("providers.codex.enabled", enabled as u8 as f64);
            let mut actions = Vec::new();
            unsafe {
                let menu = CreatePopupMenu().unwrap();
                append_context_menu_items(
                    menu,
                    &conditional,
                    localization::detect_system_language(),
                    &context,
                    None,
                    &mut actions,
                );
                let count = GetMenuItemCount(Some(menu));
                let last_id = GetMenuItemID(menu, count - 1);
                let child_count = if enabled {
                    let submenu = GetSubMenu(menu, 3);
                    Some((GetMenuItemCount(Some(submenu)), GetMenuItemID(submenu, 0)))
                } else {
                    None
                };
                DestroyMenu(menu).unwrap();
                assert_eq!(count, if enabled { 5 } else { 1 });
                assert_eq!(last_id, if enabled { 1002 } else { 1000 });
                if enabled {
                    assert_eq!(child_count, Some((1, 1001)));
                    assert_eq!(
                        actions,
                        vec![
                            ContextMenuAction::Refresh,
                            ContextMenuAction::Exit,
                            ContextMenuAction::Exit
                        ]
                    );
                } else {
                    assert_eq!(actions, vec![ContextMenuAction::Exit]);
                }
            }
        }
    }
}
