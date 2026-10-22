use super::*;
use crate::providers::ProviderId;

impl StudioApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        let mut changed = false;
        let mut requested_theme = None;
        let mut open_theme_studio = false;
        let themes = self.available_themes();
        if let Some(error) = &self.settings_error {
            ui.colored_label(egui::Color32::from_rgb(196, 64, 64), error);
            ui.add_space(8.0);
        }
        settings_scroll_area(ui, |ui| {
            section(ui, language.text("General"), |ui| {
                setting_row(
                    ui,
                    language.text("Update frequency"),
                    language.text("How often provider usage is refreshed"),
                    |ui| {
                        ui.label(language.text("minutes"));
                        let mut minutes = self.settings.poll_interval_ms / POLL_1_MIN;
                        if ui
                            .push_id(
                                ("poll_interval", self.poll_interval_editor_generation),
                                |ui| {
                                    NumberField::new(&mut minutes)
                                        .range(1..=app_settings::MAX_POLL_MINUTES)
                                        .speed(1.0)
                                        .show(ui, 100.0)
                                },
                            )
                            .inner
                            .changed()
                        {
                            self.settings.poll_interval_ms = minutes * POLL_1_MIN;
                            changed = true;
                        }
                        if ui.button(language.text("Refresh now")).clicked() {
                            self.request_refresh();
                        }
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Start with Windows"),
                    language.text("Launch the monitor when you sign in"),
                    |ui| {
                        if Toggle::new(&mut self.startup_enabled)
                            .labels(language.text("Enabled"), language.text("Disabled"))
                            .show(ui)
                            .changed()
                        {
                            crate::window::set_startup_enabled(self.startup_enabled);
                        }
                    },
                );
            });
            section(ui, language.text("Providers"), |ui| {
                for (index, descriptor) in PROVIDER_DESCRIPTORS.iter().enumerate() {
                    if index > 0 {
                        setting_separator(ui);
                    }
                    setting_row_with_mark(
                        ui,
                        provider_mark_glyph(descriptor.id),
                        language.text(descriptor.display_name),
                        language.text(descriptor.settings_description),
                        |ui| {
                            let mut enabled = self.settings.provider_enabled(descriptor.id);
                            if Toggle::new(&mut enabled)
                                .labels(language.text("Enabled"), language.text("Disabled"))
                                .show(ui)
                                .changed()
                            {
                                changed |= self.settings.toggle_provider(descriptor.id);
                            }
                        },
                    );
                    if matches!(descriptor.id, ProviderId::Claude | ProviderId::Codex) {
                        let enabled = self.settings.provider_enabled(descriptor.id);
                        let accounts = match descriptor.id {
                            ProviderId::Claude => &mut self.settings.accounts.claude,
                            _ => &mut self.settings.accounts.codex,
                        };
                        ui.push_id(descriptor.key, |ui| {
                            changed |= account_settings(
                                ui,
                                descriptor.id,
                                accounts,
                                self.usage.as_ref(),
                                enabled,
                                language,
                                self.owner,
                            );
                        });
                    }
                }
            });
            section(ui, language.text("Display"), |ui| {
                setting_row(
                    ui,
                    language.text("Usage direction"),
                    language.text("Count down what is left in supported themes"),
                    |ui| {
                        changed |= Toggle::new(&mut self.settings.usage_countdown)
                            .labels(language.text("Remaining"), language.text("Used"))
                            .show(ui)
                            .changed();
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Lock in taskbar"),
                    language.text("Keep the widget docked; never float automatically"),
                    |ui| {
                        changed |= Toggle::new(&mut self.settings.lock_taskbar)
                            .labels(language.text("Locked"), language.text("Floating"))
                            .show(ui)
                            .changed();
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Language"),
                    language.text("Language used by the app and widget"),
                    |ui| {
                        let language_code = self
                            .settings
                            .language
                            .get_or_insert_with(|| "system".into());
                        Dropdown::from_id_salt("language")
                            .width(220.0)
                            .selected_text(language_name(language, language_code))
                            .show_ui(ui, |ui| {
                                for (code, name) in languages(language) {
                                    changed |= dropdown_selectable_value(
                                        ui,
                                        language_code,
                                        code.into(),
                                        name,
                                    )
                                    .changed();
                                }
                            });
                    },
                );
            });
            section(ui, language.text("Appearance"), |ui| {
                setting_row(
                    ui,
                    language.text("Active theme"),
                    language.text("The widget is managed in Theme Studio"),
                    |ui| {
                        Dropdown::from_id_salt("active_theme")
                            .width(220.0)
                            .selected_text(&self.theme.name)
                            .show_ui(ui, |ui| {
                                for theme in themes.iter() {
                                    let selected =
                                        self.theme_path.as_deref() == Some(theme.path.as_path());
                                    if dropdown_selectable_label(ui, selected, &theme.label)
                                        .clicked()
                                    {
                                        requested_theme = Some(theme.path.clone());
                                    }
                                }
                            });
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Theme editor"),
                    language.text("Create and edit themes"),
                    |ui| {
                        if ui.button(language.text("Open Theme Studio")).clicked() {
                            open_theme_studio = true;
                        }
                    },
                );
            });
        });
        if changed {
            self.settings.accounts.claude.normalize();
            self.settings.accounts.codex.normalize();
            if let Some(data) = self.usage.as_mut() {
                data.select_accounts(&self.settings.accounts);
            }
            let new_language = self.language();
            if new_language != language {
                configure_style(ui.ctx(), new_language);
            }
            self.preview_dirty = true;
            self.save_settings();
        }
        if let Some(path) = requested_theme {
            self.request_activate_theme(path);
        }
        if open_theme_studio {
            self.page = Page::Studio;
        }
    }
}

fn account_settings(
    ui: &mut egui::Ui,
    provider: ProviderId,
    accounts: &mut crate::accounts::ProviderAccounts,
    data: Option<&AppUsageData>,
    provider_enabled: bool,
    language: LanguageId,
    owner: isize,
) -> bool {
    let mut changed = false;
    let claude = provider == ProviderId::Claude;
    if claude {
        // Blank rows from the old "Add account" button hold nothing to lose.
        let before = accounts.profiles.len();
        accounts.profiles.retain(|p| {
            p.id == "default"
                || !p.config_dir.trim().is_empty()
                || !p.credentials_path.trim().is_empty()
        });
        changed |= accounts.profiles.len() != before;
    }
    egui::CollapsingHeader::new(language.text("Accounts"))
        .default_open(true)
        .show(ui, |ui| {
            let status_id = egui::Id::new(("account_status", provider.descriptor().key));
            let mut sign_in: Option<usize> = None;
            let mut switch_to: Option<usize> = None;
            let mut sign_in_new = false;
            let mut detect = false;
            let mut add_blank = false;
            let mut remove = None;
            let live_id = if claude {
                accounts.profiles.iter().find(|p| crate::account_switch::is_live_profile(p)).map(|p| p.id.clone())
            } else { None };

            ui.label(language.text(if claude {
                "Add every Claude account you use. Choose which one VS Code and your terminals use with one click."
            } else {
                "Add each Codex account you want to track."
            }));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if claude {
                    if ui.button(language.text("+ Add another account")).clicked() { sign_in_new = true; }
                    if ui.button(language.text("Find accounts on this PC")).clicked() { detect = true; }
                } else if ui.button(language.text("+ Add account")).clicked() {
                    add_blank = true;
                }
            });
            if let Some(status) = ui.data(|data| data.get_temp::<String>(status_id)) {
                ui.add_space(4.0);
                ui.weak(status);
            }
            ui.add_space(6.0);

            for (index, profile) in accounts.profiles.iter_mut().enumerate() {
                let email = if claude { crate::account_switch::profile_email(profile) } else { None };
                if let Some(email) = &email {
                    if crate::account_switch::is_placeholder_name(&profile.name) {
                        profile.name = email.clone();
                        changed = true;
                    }
                }
                let has_folder = crate::account_switch::profile_directory(profile).is_some();
                let is_live = claude && match &live_id { Some(id) => *id == profile.id, None => !has_folder };
                let signed_in = profile.credential_path(provider).ok().flatten().is_none_or(|path| path.exists());
                let account = data.and_then(|data| data.accounts.iter().find(|account| {
                    account.provider == provider && account.profile.same_source(profile)
                }));
                ui.push_id(profile.id.clone(), |ui| {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.strong(&profile.name);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if is_live {
                                    ui.colored_label(egui::Color32::from_rgb(64, 160, 96), language.text("In use by VS Code and terminals"));
                                } else if claude && has_folder
                                    && ui.button(language.text("Use this account")).clicked() {
                                    switch_to = Some(index);
                                }
                            });
                        });
                        if let Some(email) = email.as_ref().filter(|email| **email != profile.name) {
                            ui.weak(email);
                        }
                        // One plain-language status line.
                        if !provider_enabled || !profile.enabled {
                            ui.weak(language.text("Usage tracking is off for this account"));
                        } else if claude && !signed_in {
                            ui.colored_label(egui::Color32::from_rgb(196, 112, 32), language.text("Not signed in yet. Click Sign in."));
                        } else if let Some(account) = account {
                            if let Some(error) = account.error {
                                ui.colored_label(egui::Color32::from_rgb(196, 112, 32), account_error_message(error, language));
                            }
                            if let Some(usage) = &account.usage {
                                if usage.stale { ui.weak(language.text("Last known usage")); }
                                ui.horizontal_wrapped(|ui| {
                                    for (label, section) in [("Session", &usage.session), ("Weekly", &usage.weekly)] {
                                        if section.available {
                                            ui.label(format!("{}: {:.0}%", language.text(label), section.percentage));
                                            if let Some(reset) = section.resets_at {
                                                let minutes = reset.duration_since(std::time::SystemTime::now()).unwrap_or_default().as_secs() / 60;
                                                ui.weak(format!("{} {}h {}m", language.text("Resets in"), minutes / 60, minutes % 60));
                                            }
                                        }
                                    }
                                    if let Some(credits) = &usage.credits {
                                        ui.label(format!("{}: {:.0}%", language.text("Credits"), credits.percentage));
                                    }
                                });
                            } else if account.error.is_none() {
                                ui.weak(language.text("Waiting for usage"));
                            }
                        } else {
                            ui.weak(language.text("Waiting for usage"));
                        }
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            if claude && ui.button(language.text(if signed_in { "Sign in again" } else { "Sign in" })).clicked() {
                                sign_in = Some(index);
                            }
                            changed |= ui.checkbox(&mut profile.enabled, language.text("Track usage")).changed();
                            if ui.button(language.text("Remove"))
                                .on_hover_text(language.text("Stops tracking this account. It does not sign you out."))
                                .clicked() {
                                remove = Some(index);
                            }
                        });
                        ui.collapsing(language.text("Advanced"), |ui| {
                            ui.label(language.text("Display name"));
                            let name = ui.add(crate::ui::components::text_field::singleline(&mut profile.name)
                                .desired_width(260.0).hint_text(language.text("Account name")));
                            changed |= name.lost_focus();
                            ui.label(language.text("Login folder"));
                            let directory = ui.add(crate::ui::components::text_field::singleline(&mut profile.config_dir)
                                .hint_text(if claude { "~/.claude-work" } else { "~/.codex-work" }));
                            changed |= directory.lost_focus();
                            ui.label(language.text("Login file (optional, overrides the folder)"));
                            let file = ui.add(crate::ui::components::text_field::singleline(&mut profile.credentials_path));
                            changed |= file.lost_focus();
                            if ui.button(language.text("Browse...")).clicked() {
                                if let Some(path) = choose_file(owner, language.text("Select credentials file"), "JSON files\0*.json\0All files\0*.*\0\0") {
                                    profile.credentials_path = path.to_string_lossy().into_owned();
                                    changed = true;
                                }
                            }
                            match profile.credential_path(provider) {
                                Err(error) => { ui.colored_label(egui::Color32::from_rgb(196, 64, 64), error); }
                                Ok(None) => { ui.weak(language.text("Uses this PC's main login")); }
                                Ok(Some(path)) => { ui.weak(path.display().to_string()); }
                            }
                            ui.weak(format!("{}: accounts.{}.{}", language.text("Theme binding"), provider.descriptor().key, profile.id));
                        });
                    });
                });
                ui.add_space(4.0);
            }

            let mut status: Option<String> = None;
            if let Some(index) = remove {
                accounts.profiles.remove(index);
                changed = true;
            }
            if add_blank {
                accounts.add();
                changed = true;
            }
            if sign_in_new {
                accounts.add();
                if let Some(profile) = accounts.profiles.last_mut() {
                    profile.enabled = true;
                    profile.config_dir = crate::account_switch::suggested_directory(&profile.id);
                    status = Some(start_sign_in(profile));
                }
                changed = true;
            }
            if let Some(profile) = sign_in.and_then(|index| accounts.profiles.get_mut(index)) {
                if profile.id != "default" && profile.config_dir.trim().is_empty() && profile.credentials_path.trim().is_empty() {
                    profile.config_dir = crate::account_switch::suggested_directory(&profile.id);
                }
                profile.enabled = true;
                status = Some(start_sign_in(profile));
                changed = true;
            }
            if detect {
                let found = crate::account_switch::unmonitored_profiles(&accounts.profiles);
                status = Some(match found.len() {
                    0 => "No new accounts found on this PC.".to_string(),
                    count => {
                        for (name, config_dir) in found {
                            accounts.add();
                            if let Some(profile) = accounts.profiles.last_mut() {
                                profile.name = name;
                                profile.config_dir = config_dir;
                                profile.enabled = true;
                            }
                        }
                        changed = true;
                        format!("Added {count} account(s) found on this PC.")
                    }
                });
            }
            if let Some(index) = switch_to {
                status = Some(match crate::account_switch::swap_live_account(accounts, index) {
                    Ok(message) => { changed = true; message }
                    Err(error) => format!("Could not switch: {error}"),
                });
            }
            if let Some(status) = status {
                ui.data_mut(|data| data.insert_temp(status_id, status));
            }

            ui.collapsing(language.text("Widget options"), |ui| {
                ui.weak(language.text("Themes use this account unless they specify another. Custom themes can display several accounts."));
                ui.horizontal(|ui| {
                    ui.label(language.text("Widget shows"));
                    let selected = accounts.selected().map(|p| p.name.as_str()).unwrap_or("None");
                    Dropdown::from_id_salt("widget_account")
                        .width(220.0)
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for profile in accounts.profiles.iter().filter(|p| p.enabled) {
                                changed |= dropdown_selectable_value(ui, &mut accounts.selected,
                                    profile.id.clone(), &profile.name).changed();
                            }
                        });
                });
            });
        });
    changed
}

fn start_sign_in(profile: &crate::accounts::AccountProfile) -> String {
    let directory = crate::account_switch::profile_directory(profile);
    match crate::account_switch::launch_login(directory.as_deref()) {
        Ok(()) => format!(
            "A sign-in window opened for {}. Finish signing in there; usage appears here automatically.",
            profile.name
        ),
        Err(error) => format!("Could not start sign-in: {error}. Is the Claude CLI installed?"),
    }
}

fn account_error_message(error: crate::poller::PollError, language: LanguageId) -> String {
    error.message(language)
}

#[cfg(test)]
mod account_status_tests {
    use super::*;

    #[test]
    fn status_explains_http_failure_and_correct_recovery_action() {
        use crate::poller::PollError;
        assert_eq!(
            account_error_message(PollError::HttpStatus(429), LanguageId::English),
            "HTTP 429: Too Many Requests. Retrying at the next refresh"
        );
        assert_eq!(
            account_error_message(PollError::HttpStatus(401), LanguageId::English),
            "HTTP 401: Unauthorized. Sign in again for this account"
        );
        assert!(
            account_error_message(PollError::HttpStatus(503), LanguageId::English)
                .contains("Service Unavailable")
        );
    }
}
