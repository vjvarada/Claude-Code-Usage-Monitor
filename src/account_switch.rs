//! Claude account setup helpers: find existing logins, start a sign-in for a
//! new profile, and make one profile the account used by the CLI and VS Code.
//! Only paths and e-mail labels are handled here, never tokens.
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::accounts::{expand_path, source_key, AccountProfile};

const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const ENVIRONMENT_VARIABLE: &str = "CLAUDE_CONFIG_DIR";
const VSCODE_SETTING: &str = "claudeCode.environmentVariables";
/// VS Code builds that host the Claude Code extension.
const EDITOR_FOLDERS: [&str; 3] = ["Code", "Code - Insiders", "Cursor"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredAccount {
    pub config_dir: PathBuf,
    pub email: Option<String>,
}

/// What clearing the folder override did, for the switch status message.
#[derive(Clone, Debug, Default)]
pub struct SwitchReport {
    pub environment: Option<Result<(), String>>,
    pub editors: Vec<(&'static str, Result<bool, String>)>,
}

/// The directory that holds this profile's logins, or `None` for the default
/// `~/.claude` account.
pub fn profile_directory(profile: &AccountProfile) -> Option<PathBuf> {
    let directory = if !profile.config_dir.trim().is_empty() {
        expand_path(Path::new(profile.config_dir.trim()))
    } else if !profile.credentials_path.trim().is_empty() {
        expand_path(Path::new(profile.credentials_path.trim()))?
            .parent()
            .map(Path::to_path_buf)
    } else {
        None
    }?;
    (!is_default_directory(&directory)).then_some(directory)
}

fn is_default_directory(directory: &Path) -> bool {
    crate::known_folders::home_dir()
        .is_some_and(|home| source_key(&home.join(".claude")) == source_key(directory))
}

/// `~/name` for folders under the user profile so settings stay portable.
pub fn portable_path(path: &Path) -> String {
    crate::known_folders::home_dir()
        .and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        .map(|tail| format!("~/{}", tail.to_string_lossy().replace('\\', "/")))
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub fn suggested_directory(profile_id: &str) -> String {
    format!("~/.claude-{}", profile_id.replace('_', "-"))
}

/// Folders in the user profile named `.claude*` that contain a login.
pub fn discover_accounts() -> Vec<DiscoveredAccount> {
    let Some(home) = crate::known_folders::home_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&home) else {
        return Vec::new();
    };
    let mut found: Vec<DiscoveredAccount> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            name.starts_with(".claude")
                && name != ".claude"
                && entry.path().is_dir()
                && entry.path().join(".credentials.json").is_file()
        })
        .map(|entry| DiscoveredAccount {
            email: account_email(&entry.path()),
            config_dir: entry.path(),
        })
        .collect();
    found.sort_by(|a, b| a.config_dir.cmp(&b.config_dir));
    found
}

type EmailCache = HashMap<PathBuf, (Option<SystemTime>, Option<String>)>;
static EMAIL_CACHE: Mutex<Option<EmailCache>> = Mutex::new(None);

/// The signed-in e-mail recorded by Claude Code. `None` directory means the
/// default account. Reads are cached until the file changes.
pub fn account_email(directory: &Path) -> Option<String> {
    let mut candidates = vec![directory.join(".claude.json")];
    if is_default_directory(directory) {
        candidates.push(crate::known_folders::home_dir()?.join(".claude.json"));
    }
    candidates.into_iter().find_map(|path| cached_email(&path))
}

pub fn default_account_email() -> Option<String> {
    account_email(&crate::known_folders::home_dir()?.join(".claude"))
}

pub fn profile_email(profile: &AccountProfile) -> Option<String> {
    match profile_directory(profile) {
        Some(directory) => account_email(&directory),
        // Only the built-in profile is the main login; a blank profile is not.
        None if profile.id == "default" => default_account_email(),
        None => None,
    }
}

/// Accounts found on disk that no profile monitors yet, as ready-to-add profiles.
pub fn unmonitored_profiles(existing: &[AccountProfile]) -> Vec<(String, String)> {
    let known_emails: Vec<String> = existing.iter().filter_map(profile_email).collect();
    discover_accounts()
        .into_iter()
        .filter(|found| {
            // The same login saved in a second folder is a duplicate, not a new account.
            !found.email.as_ref().is_some_and(|email| {
                known_emails
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(email))
            }) && !existing.iter().any(|profile| {
                profile_directory(profile).is_some_and(|directory| {
                    source_key(&directory) == source_key(&found.config_dir)
                })
            })
        })
        .map(|found| {
            let name = found.email.clone().unwrap_or_else(|| {
                found
                    .config_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().trim_start_matches('.').to_string())
                    .unwrap_or_default()
            });
            (name, portable_path(&found.config_dir))
        })
        .collect()
}

/// `Account 3`-style placeholders that may be replaced by the signed-in e-mail.
pub fn is_placeholder_name(name: &str) -> bool {
    name == "Default"
        || name
            .strip_prefix("Account ")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn cached_email(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok();
    let mut guard = EMAIL_CACHE.lock().ok()?;
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((cached_at, email)) = cache.get(path) {
        if *cached_at == modified {
            return email.clone();
        }
    }
    let email = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|json| {
            json.pointer("/oauthAccount/emailAddress")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    cache.insert(path.to_path_buf(), (modified, email.clone()));
    email
}

/// Opens a console running `claude /login` against `directory`. The poller
/// notices the new credentials file on its own.
pub fn launch_login(directory: Option<&Path>) -> Result<(), String> {
    let mut command = std::process::Command::new("cmd.exe");
    command
        .args(["/d", "/k", "claude", "/login"])
        .creation_flags(CREATE_NEW_CONSOLE)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    match directory {
        Some(directory) => {
            std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
            command.env(ENVIRONMENT_VARIABLE, directory);
        }
        None => {
            command.env_remove(ENVIRONMENT_VARIABLE);
        }
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// The account new terminals and editors use, from the user environment.
/// `None` means the default `~/.claude`.
pub fn active_directory() -> Option<PathBuf> {
    read_user_variable(ENVIRONMENT_VARIABLE)
        .filter(|value| !value.is_empty())
        .and_then(|value| expand_path(Path::new(&value)))
        .filter(|directory| !is_default_directory(directory))
}

/// Makes `directory` (or the default account for `None`) the account used by
/// new terminals, the Claude CLI and the VS Code extension.
pub fn activate(directory: Option<&Path>) -> SwitchReport {
    let mut report = SwitchReport {
        environment: Some(set_user_variable(
            ENVIRONMENT_VARIABLE,
            directory.map(|path| path.to_string_lossy().into_owned()),
        )),
        ..Default::default()
    };
    if let Some(roaming) = crate::known_folders::roaming_app_data_dir() {
        for folder in EDITOR_FOLDERS {
            let settings = roaming.join(folder).join("User").join("settings.json");
            if settings.parent().is_some_and(Path::is_dir) {
                report.editors.push((
                    folder,
                    update_editor_settings(&settings, directory.map(|p| p.to_string_lossy())),
                ));
            }
        }
    }
    report
}

fn update_editor_settings(
    path: &Path,
    directory: Option<std::borrow::Cow<'_, str>>,
) -> Result<bool, String> {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let Some(updated) = patch_environment_variables(&original, directory.as_deref())? else {
        return Ok(false);
    };
    if !original.trim().is_empty() {
        let backup = path.with_file_name("settings.json.usage-monitor.bak");
        if !backup.exists() {
            let _ = std::fs::copy(path, backup);
        }
    }
    let temporary = path.with_file_name("settings.json.usage-monitor.tmp");
    std::fs::write(&temporary, updated).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        e.to_string()
    })?;
    Ok(true)
}

fn read_user_variable(name: &str) -> Option<String> {
    unsafe {
        let mut key = HKEY::default();
        let path = crate::native_interop::wide_str("Environment");
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut key,
        )
        .ok()
        .ok()?;
        let name = crate::native_interop::wide_str(name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        let probe = RegQueryValueExW(
            key,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        );
        let value = if probe == ERROR_SUCCESS && (kind == REG_SZ || kind == REG_EXPAND_SZ) {
            let mut buffer = vec![0u16; size as usize / 2 + 1];
            let mut size = (buffer.len() * 2) as u32;
            (RegQueryValueExW(
                key,
                PCWSTR::from_raw(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(buffer.as_mut_ptr() as *mut u8),
                Some(&mut size),
            ) == ERROR_SUCCESS)
                .then(|| {
                    let length = (size as usize / 2).min(buffer.len());
                    String::from_utf16_lossy(&buffer[..length])
                        .trim_end_matches('\0')
                        .to_string()
                })
        } else {
            None
        };
        let _ = RegCloseKey(key);
        value
    }
}

fn set_user_variable(name: &str, value: Option<String>) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        let path = crate::native_interop::wide_str("Environment");
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut key,
        )
        .ok()
        .map_err(|e| e.to_string())?;
        let name_wide = crate::native_interop::wide_str(name);
        let result = match value {
            Some(value) => {
                let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
                RegSetValueExW(
                    key,
                    PCWSTR::from_raw(name_wide.as_ptr()),
                    None,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        wide.as_ptr() as *const u8,
                        wide.len() * 2,
                    )),
                )
                .ok()
                .map_err(|e| e.to_string())
            }
            None => {
                let result = RegDeleteValueW(key, PCWSTR::from_raw(name_wide.as_ptr()));
                // A variable that was never set is already the default.
                if result == ERROR_SUCCESS || result.0 == 2 {
                    Ok(())
                } else {
                    result.ok().map_err(|e| e.to_string())
                }
            }
        };
        let _ = RegCloseKey(key);
        if result.is_ok() {
            // Tell Explorer so terminals and editors started afterwards inherit it.
            let environment = crate::native_interop::wide_str("Environment");
            let _ = SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                Default::default(),
                windows::Win32::Foundation::LPARAM(environment.as_ptr() as isize),
                SMTO_ABORTIFHUNG,
                2_000,
                None,
            );
        }
        result
    }
}

/// `~/.claude`: the account every terminal and VS Code window uses when no
/// `CLAUDE_CONFIG_DIR` is set.
pub fn live_credentials_path() -> Option<PathBuf> {
    Some(
        crate::known_folders::home_dir()?
            .join(".claude")
            .join(".credentials.json"),
    )
}

fn live_global_config() -> Option<PathBuf> {
    Some(crate::known_folders::home_dir()?.join(".claude.json"))
}

/// True when this profile's saved login is the account currently signed in at
/// `~/.claude`. Decided by e-mail, so a manual `/login` inside a session is
/// recognised too.
pub fn is_live_profile(profile: &AccountProfile) -> bool {
    let Some(directory) = profile_directory(profile) else {
        return false;
    };
    match (account_email(&directory), default_account_email()) {
        (Some(saved), Some(live)) => saved.eq_ignore_ascii_case(&live),
        _ => false,
    }
}

/// Index of the profile whose login is live in `~/.claude`. Falls back to the
/// built-in profile when the live login matches no saved folder.
pub fn live_profile_index(profiles: &[AccountProfile]) -> Option<usize> {
    profiles.iter().position(is_live_profile).or_else(|| {
        profiles
            .iter()
            .position(|p| profile_directory(p).is_none() && p.id == "default")
    })
}

/// Usage percentage at which the live account is considered close to a limit.
pub const WARN_AT_PERCENT: f64 = 90.0;

/// "2h 5m" / "1d 4h" until `reset`, or `None` when unknown.
pub fn format_reset(reset: Option<SystemTime>) -> Option<String> {
    let minutes = reset?
        .duration_since(SystemTime::now())
        .unwrap_or_default()
        .as_secs()
        / 60;
    Some(match minutes {
        0..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60),
    })
}

/// `95% (resets 2h 5m)` for one usage window.
pub fn describe_window(section: &crate::models::UsageSection) -> String {
    match format_reset(section.resets_at) {
        Some(reset) => format!("{:.0}% (resets {reset})", section.percentage),
        None => format!("{:.0}%", section.percentage),
    }
}

/// How close an account is to being unusable: the fuller of its two windows.
fn pressure(usage: &crate::models::UsageData) -> f64 {
    [&usage.session, &usage.weekly]
        .into_iter()
        .filter(|section| section.available)
        .map(|section| section.percentage)
        .fold(0.0, f64::max)
}

pub struct Suggestion {
    pub title: String,
    pub body: String,
    /// Same value while the same limit is being approached, so it is only announced once.
    pub key: String,
}

/// When the live account is nearly out of session or weekly allowance, names
/// the other tracked account with the most room left. `None` when the live
/// account is fine.
pub fn suggest_switch(
    profiles: &[AccountProfile],
    accounts: &[crate::models::AccountUsage],
) -> Option<Suggestion> {
    let usage_of = |profile: &AccountProfile| {
        accounts
            .iter()
            .find(|a| {
                a.provider == crate::providers::ProviderId::Claude && a.profile.same_source(profile)
            })
            .and_then(|a| a.usage.as_ref())
    };
    let live = live_profile_index(profiles)?;
    let live_usage = usage_of(&profiles[live])?;
    let (window, section) = [
        ("session", &live_usage.session),
        ("weekly", &live_usage.weekly),
    ]
    .into_iter()
    .filter(|(_, s)| s.available && s.percentage >= WARN_AT_PERCENT)
    .max_by(|a, b| a.1.percentage.total_cmp(&b.1.percentage))?;
    let best = profiles
        .iter()
        .enumerate()
        .filter(|(index, p)| *index != live && p.enabled && profile_directory(p).is_some())
        .filter_map(|(_, p)| usage_of(p).map(|usage| (p, usage, pressure(usage))))
        .filter(|(_, _, pressure)| *pressure < WARN_AT_PERCENT)
        .min_by(|a, b| a.2.total_cmp(&b.2));
    let live_name = &profiles[live].name;
    let reset = format_reset(section.resets_at)
        .map(|reset| format!(", resets in {reset}"))
        .unwrap_or_default();
    let body = match best {
        Some((profile, usage, _)) => format!(
            "{live_name} is at {:.0}% of its {window} limit{reset}. Suggested: switch to {} (session {}, weekly {}). Right-click the widget to switch.",
            section.percentage,
            profile.name,
            describe_window(&usage.session),
            describe_window(&usage.weekly),
        ),
        None => format!(
            "{live_name} is at {:.0}% of its {window} limit{reset}. No other tracked account has room left.",
            section.percentage
        ),
    };
    Some(Suggestion {
        title: "Claude usage limit approaching".into(),
        body,
        key: format!(
            "{}:{window}:{}",
            profiles[live].id,
            best.map_or("", |b| b.0.id.as_str())
        ),
    })
}

static LAST_SUGGESTION: Mutex<Option<String>> = Mutex::new(None);

/// Returns the suggestion only the first time it appears; clears once usage recovers.
pub fn announce_once(suggestion: Option<Suggestion>) -> Option<Suggestion> {
    let mut last = LAST_SUGGESTION.lock().ok()?;
    match suggestion {
        Some(s) if last.as_deref() != Some(s.key.as_str()) => {
            *last = Some(s.key.clone());
            Some(s)
        }
        Some(_) => None,
        None => {
            *last = None;
            None
        }
    }
}

fn read_oauth_account(path: &Path) -> Option<Value> {
    let json: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let account = json.get("oauthAccount")?.clone();
    account.get("emailAddress")?.as_str()?;
    Some(account)
}

/// Replaces only `oauthAccount`, keeping every other key of a Claude config.
fn write_oauth_account(path: &Path, account: &Value) -> Result<(), String> {
    let mut json: Value = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| format!("{} is not valid JSON; left unchanged", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.to_string()),
    };
    json.as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?
        .insert("oauthAccount".into(), account.clone());
    atomic_write(
        path,
        &serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())?,
    )
}

/// Write beside the target and rename, retrying briefly if Claude Code has the
/// file open at that instant.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("usage-monitor.tmp");
    std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    let mut last = String::new();
    for _ in 0..5 {
        match std::fs::rename(&temporary, path) {
            Ok(()) => return Ok(()),
            Err(error) => last = error.to_string(),
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
    let _ = std::fs::remove_file(&temporary);
    Err(last)
}

fn backup_once(path: &Path) {
    if path.is_file() {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".usage-monitor.bak");
        let backup = path.with_file_name(name);
        if !backup.exists() {
            let _ = std::fs::copy(path, backup);
        }
    }
}

fn copy_login(from: &Path, to: &Path) -> Result<(), String> {
    let bytes = std::fs::read(from).map_err(|e| e.to_string())?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    atomic_write(to, &bytes)
}

type McpLogins = serde_json::Map<String, Value>;

fn mcp_logins(credentials: &Value) -> McpLogins {
    credentials
        .get("mcpOAuth")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Adds `from` into `into`, keeping whichever copy of a server's login expires
/// later (the most recently refreshed one). On a tie `from` wins.
fn merge_mcp_logins(into: &mut McpLogins, from: &McpLogins) {
    let expiry = |entry: &Value| entry.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    for (server, entry) in from {
        if into
            .get(server)
            .is_none_or(|current| expiry(entry) >= expiry(current))
        {
            into.insert(server.clone(), entry.clone());
        }
    }
}

/// The target's credentials with MCP server logins carried over: MCP servers
/// are tied to this PC's setup, not to one Claude account, so signing in to
/// one from any account must keep working after a switch.
fn credentials_with_shared_mcp(
    target: &Path,
    outgoing: Option<&Path>,
    others: &[PathBuf],
) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(target).map_err(|e| e.to_string())?;
    let Ok(mut credentials) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(bytes);
    };
    let read = |path: &Path| {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(|v| mcp_logins(&v))
    };
    let mut merged = McpLogins::new();
    for path in others {
        if let Some(logins) = read(path) {
            merge_mcp_logins(&mut merged, &logins);
        }
    }
    merge_mcp_logins(&mut merged, &mcp_logins(&credentials));
    if let Some(logins) = outgoing.and_then(read) {
        merge_mcp_logins(&mut merged, &logins);
    }
    let Some(object) = credentials.as_object_mut() else {
        return Ok(bytes);
    };
    if merged.is_empty() {
        return Ok(bytes);
    }
    object.insert("mcpOAuth".into(), Value::Object(merged));
    serde_json::to_vec(&credentials).map_err(|e| e.to_string())
}

/// Makes `accounts.profiles[target]` the single signed-in account of
/// `~/.claude`, which running Claude Code sessions and new terminals share.
///
/// The outgoing login is saved first (into its own profile, creating one when
/// none matches) so no token is ever lost, and any `CLAUDE_CONFIG_DIR` left by
/// the older per-folder switch is cleared so everything agrees on `~/.claude`.
pub fn swap_live_account(
    accounts: &mut crate::accounts::ProviderAccounts,
    target: usize,
) -> Result<String, String> {
    let live_credentials = live_credentials_path().ok_or("Cannot locate your user folder")?;
    let global = live_global_config().ok_or("Cannot locate your user folder")?;
    let environment_was_set = active_directory().is_some();
    let (name, already_live) = swap_files(
        &live_credentials,
        &global,
        accounts,
        target,
        suggested_directory,
    )?;
    // One source of truth: no folder override for terminals or editors.
    let report = activate(None);
    let mut text = if already_live {
        format!("{name} is already the live account.")
    } else {
        format!("Switched to {name}.")
    };
    text.push_str(" Open Claude Code sessions use it on their next request; if one still shows the old account, reload that VS Code window.");
    if environment_was_set {
        text.push_str(
            " A folder override was cleared, so restart sessions that were started under it.",
        );
    }
    if let Some(Err(error)) = report.environment {
        text.push_str(&format!(
            " Could not clear the CLAUDE_CONFIG_DIR override: {error}."
        ));
    }
    for (editor, result) in report.editors {
        if let Err(error) = result {
            text.push_str(&format!(" {editor} settings: {error}."));
        }
    }
    Ok(text)
}

/// File and profile half of the switch. Returns the target's name and whether
/// it was already live.
fn swap_files(
    live_credentials: &Path,
    global: &Path,
    accounts: &mut crate::accounts::ProviderAccounts,
    target: usize,
    new_directory: impl Fn(&str) -> String,
) -> Result<(String, bool), String> {
    let target_profile = accounts
        .profiles
        .get(target)
        .ok_or("Account not found")?
        .clone();
    let target_directory = profile_directory(&target_profile)
        .ok_or("This account has no folder of its own; sign it in first")?;
    let target_credentials = target_directory.join(".credentials.json");
    if !target_credentials.is_file() {
        return Err(format!("{} is not signed in yet", target_profile.name));
    }
    let target_account = read_oauth_account(&target_directory.join(".claude.json"))
        .ok_or("Sign this account in again so the app can tell which account it is")?;
    let target_email = target_account["emailAddress"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let live_account = read_oauth_account(global);
    let live_email = live_account
        .as_ref()
        .and_then(|account| account["emailAddress"].as_str().map(str::to_string));

    let already_live = live_email
        .as_deref()
        .is_some_and(|email| email.eq_ignore_ascii_case(&target_email))
        && live_credentials.is_file();
    if !already_live {
        // 1. Save the outgoing login. The live copy is authoritative because
        //    its refresh token may have rotated since it was last saved.
        if live_credentials.is_file() {
            let existing = live_email.as_deref().and_then(|email| {
                accounts.profiles.iter().position(|profile| {
                    profile_directory(profile).is_some()
                        && profile_email(profile)
                            .is_some_and(|saved| saved.eq_ignore_ascii_case(email))
                })
            });
            let directory = match existing {
                Some(index) => profile_directory(&accounts.profiles[index]).unwrap_or_default(),
                None => {
                    accounts.add();
                    let profile = accounts
                        .profiles
                        .last_mut()
                        .ok_or("Cannot save current login")?;
                    profile.name = live_email
                        .clone()
                        .unwrap_or_else(|| "Previous login".into());
                    profile.enabled = true;
                    profile.config_dir = new_directory(&profile.id);
                    profile_directory(profile).ok_or("Cannot save current login")?
                }
            };
            copy_login(live_credentials, &directory.join(".credentials.json"))
                .map_err(|e| format!("Could not save the current login: {e}"))?;
            if let Some(account) = &live_account {
                write_oauth_account(&directory.join(".claude.json"), account)?;
            }
        }
        // 2. Install the target. Keep one pristine backup of the originals.
        backup_once(live_credentials);
        backup_once(global);
        let outgoing = std::fs::read(live_credentials).ok();
        let others: Vec<PathBuf> = accounts
            .profiles
            .iter()
            .filter_map(profile_directory)
            .map(|directory| directory.join(".credentials.json"))
            .filter(|path| *path != target_credentials)
            .collect();
        credentials_with_shared_mcp(&target_credentials, Some(live_credentials), &others)
            .and_then(|bytes| atomic_write(live_credentials, &bytes))
            .map_err(|e| format!("Could not install the new login: {e}"))?;
        if let Err(error) = write_oauth_account(global, &target_account) {
            // Never leave new tokens next to the old account's identity.
            if let Some(bytes) = outgoing {
                let _ = atomic_write(live_credentials, &bytes);
            }
            return Err(error);
        }
    }
    accounts.live_swap = true;
    accounts.selected = target_profile.id.clone();
    for profile in &mut accounts.profiles {
        // The unpinned Default profile would now duplicate the live account.
        if profile.id == "default" && profile_directory(profile).is_none() {
            profile.enabled = false;
        }
    }
    Ok((target_profile.name, already_live))
}

/// Sets or removes `CLAUDE_CONFIG_DIR` inside the `claudeCode.environmentVariables`
/// array of a VS Code settings file while leaving comments, formatting and
/// every other setting untouched. `Ok(None)` means nothing needed to change.
fn patch_environment_variables(
    text: &str,
    directory: Option<&str>,
) -> Result<Option<String>, String> {
    let layout = scan_settings(text)?;
    let mut entries: Vec<Value> = match &layout.value {
        Some((start, end)) => serde_json::from_str(&strip_jsonc(&text[*start..*end]))
            .map_err(|_| format!("{VSCODE_SETTING} is not a valid list"))?,
        None => Vec::new(),
    };
    let before = entries.clone();
    entries.retain(|entry| entry.get("name").and_then(Value::as_str) != Some(ENVIRONMENT_VARIABLE));
    if let Some(directory) = directory {
        entries.push(serde_json::json!({"name": ENVIRONMENT_VARIABLE, "value": directory}));
    }
    if entries == before && layout.value.is_some() {
        return Ok(None);
    }
    if entries.is_empty() && layout.value.is_none() {
        return Ok(None);
    }
    let rendered = serde_json::to_string_pretty(&entries)
        .map_err(|e| e.to_string())?
        .replace('\n', "\n    ");
    Ok(Some(match layout.value {
        Some((start, end)) => format!("{}{rendered}{}", &text[..start], &text[end..]),
        None => match layout.close {
            // Insert as the last property of the root object.
            Some(close) => {
                let comma = match layout.last_significant {
                    Some(index) if !matches!(text.as_bytes()[index], b'{' | b',') => {
                        (index + 1, ",")
                    }
                    _ => (close, ""),
                };
                let mut out = text[..comma.0].to_string();
                out.push_str(comma.1);
                out.push_str(&text[comma.0..close]);
                out.push_str(&format!("    \"{VSCODE_SETTING}\": {rendered}\n"));
                out.push_str(&text[close..]);
                out
            }
            None if text.trim().is_empty() => {
                format!("{{\n    \"{VSCODE_SETTING}\": {rendered}\n}}\n")
            }
            None => return Err("settings.json has no top-level object".into()),
        },
    }))
}

struct SettingsLayout {
    /// Byte range of the existing array value.
    value: Option<(usize, usize)>,
    /// Position of the root object's closing brace.
    close: Option<usize>,
    /// Last token before that brace that is not whitespace or a comment.
    last_significant: Option<usize>,
}

fn skip_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        match bytes.get(index) {
            Some(b) if b.is_ascii_whitespace() => index += 1,
            Some(b'/') if bytes.get(index + 1) == Some(&b'/') => {
                while bytes.get(index).is_some_and(|b| *b != b'\n') {
                    index += 1;
                }
            }
            Some(b'/') if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index < bytes.len() && !bytes[index..].starts_with(b"*/") {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            _ => return index,
        }
    }
}

fn skip_string(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while let Some(byte) = bytes.get(index) {
        index += 1;
        match byte {
            b'\\' => index += 1,
            b'"' => break,
            _ => {}
        }
    }
    index.min(bytes.len())
}

/// End of the JSON value that starts at `index` (after trivia).
fn skip_value(bytes: &[u8], index: usize) -> usize {
    match bytes.get(index) {
        Some(b'"') => skip_string(bytes, index),
        Some(b'[' | b'{') => {
            let mut depth = 0usize;
            let mut i = index;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => {
                        i = skip_string(bytes, i);
                        continue;
                    }
                    b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => {
                        i = skip_trivia(bytes, i);
                        continue;
                    }
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            bytes.len()
        }
        _ => {
            let mut i = index;
            while bytes
                .get(i)
                .is_some_and(|b| !matches!(b, b',' | b'}' | b']') && !b.is_ascii_whitespace())
            {
                i += 1;
            }
            i
        }
    }
}

fn scan_settings(text: &str) -> Result<SettingsLayout, String> {
    let bytes = text.as_bytes();
    let mut layout = SettingsLayout {
        value: None,
        close: None,
        last_significant: None,
    };
    let mut index = skip_trivia(bytes, 0);
    if index >= bytes.len() {
        return Ok(layout);
    }
    if bytes[index] != b'{' {
        return Err("settings.json is not a JSON object".into());
    }
    layout.last_significant = Some(index);
    index += 1;
    loop {
        index = skip_trivia(bytes, index);
        match bytes.get(index) {
            None => return Err("settings.json is incomplete".into()),
            Some(b'}') => {
                layout.close = Some(index);
                return Ok(layout);
            }
            Some(b',') => {
                layout.last_significant = Some(index);
                index += 1;
            }
            Some(b'"') => {
                let end = skip_string(bytes, index);
                let key = &text[index + 1..end.saturating_sub(1).max(index + 1)];
                let colon = skip_trivia(bytes, end);
                if bytes.get(colon) != Some(&b':') {
                    return Err("settings.json could not be parsed".into());
                }
                let start = skip_trivia(bytes, colon + 1);
                let value_end = skip_value(bytes, start);
                if key == VSCODE_SETTING {
                    if bytes.get(start) != Some(&b'[') {
                        return Err(format!("{VSCODE_SETTING} is not a list"));
                    }
                    layout.value = Some((start, value_end));
                }
                layout.last_significant = Some(value_end.saturating_sub(1));
                index = value_end;
            }
            Some(_) => return Err("settings.json could not be parsed".into()),
        }
    }
}

/// Removes comments and trailing commas so `serde_json` can read VS Code's
/// JSON-with-comments syntax.
fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let end = skip_string(bytes, i);
                out.extend_from_slice(&bytes[i..end]);
                i = end;
            }
            b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => i = skip_trivia(bytes, i),
            b',' => {
                let next = skip_trivia(bytes, i + 1);
                if !matches!(bytes.get(next), Some(b']' | b'}')) {
                    out.push(b',');
                }
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patched(text: &str, directory: Option<&str>) -> String {
        patch_environment_variables(text, directory)
            .unwrap()
            .unwrap()
    }

    fn entries(text: &str) -> Vec<Value> {
        let layout = scan_settings(text).unwrap();
        let (start, end) = layout.value.unwrap();
        serde_json::from_str(&strip_jsonc(&text[start..end])).unwrap()
    }

    #[test]
    fn adds_the_setting_to_an_empty_or_missing_file() {
        let text = patched("", Some("C:\\work"));
        assert_eq!(entries(&text)[0]["value"], "C:\\work");
        assert!(serde_json::from_str::<Value>(&text).is_ok());
        assert_eq!(patch_environment_variables("", None).unwrap(), None);
        assert_eq!(patch_environment_variables("{}", None).unwrap(), None);
    }

    #[test]
    fn keeps_comments_other_settings_and_other_variables() {
        let original = "{\n    // keep me\n    \"editor.fontSize\": 14, /* inline */\n    \"claudeCode.environmentVariables\": [\n        {\"name\": \"HTTP_PROXY\", \"value\": \"x\"}, // proxy\n        {\"name\": \"CLAUDE_CONFIG_DIR\", \"value\": \"C:\\\\old\"},\n    ],\n    \"files.autoSave\": \"off\",\n}\n";
        let text = patched(original, Some("C:\\new"));
        assert!(text.contains("// keep me") && text.contains("/* inline */"));
        assert!(text.contains("\"files.autoSave\": \"off\""));
        let list = entries(&text);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "HTTP_PROXY");
        assert_eq!(list[1]["value"], "C:\\new");
        // Switching to the default account removes only our variable.
        let text = patched(&text, None);
        let list = entries(&text);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "HTTP_PROXY");
        assert_eq!(patch_environment_variables(&text, None).unwrap(), None);
    }

    #[test]
    fn inserts_after_the_last_property_with_or_without_a_trailing_comma() {
        for original in [
            "{\n    \"a\": 1\n}",
            "{\n    \"a\": 1,\n}",
            "{\n    \"a\": [1, 2] // note\n}",
            "{\n    // only a comment\n}",
        ] {
            let text = patched(original, Some("D:\\acct"));
            let json: Value = serde_json::from_str(&strip_jsonc(&text)).unwrap();
            assert_eq!(json[VSCODE_SETTING][0]["value"], "D:\\acct", "{original}");
        }
    }

    #[test]
    fn refuses_to_touch_files_it_cannot_parse() {
        assert!(patch_environment_variables("[1, 2]", Some("C:\\a")).is_err());
        assert!(patch_environment_variables("{ \"a\" 1 }", Some("C:\\a")).is_err());
        assert!(patch_environment_variables(
            "{ \"claudeCode.environmentVariables\": 5 }",
            Some("C:\\a")
        )
        .is_err());
    }

    #[test]
    fn profile_directories_resolve_from_folder_or_credentials_file() {
        assert_eq!(profile_directory(&AccountProfile::default()), None);
        let work = AccountProfile {
            id: "work".into(),
            config_dir: "C:\\accounts\\work".into(),
            ..Default::default()
        };
        assert_eq!(
            profile_directory(&work),
            Some(PathBuf::from("C:\\accounts\\work"))
        );
        let by_file = AccountProfile {
            id: "file".into(),
            credentials_path: "C:\\accounts\\work\\.credentials.json".into(),
            ..Default::default()
        };
        assert_eq!(
            profile_directory(&by_file),
            Some(PathBuf::from("C:\\accounts\\work"))
        );
        assert_eq!(suggested_directory("account_3"), "~/.claude-account-3");
    }

    fn write_login(directory: &Path, email: &str, token: &str) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join(".credentials.json"), token).unwrap();
        std::fs::write(
            directory.join(".claude.json"),
            format!(r#"{{"oauthAccount":{{"emailAddress":"{email}"}}}}"#),
        )
        .unwrap();
    }

    fn profile(root: &Path, id: &str) -> AccountProfile {
        AccountProfile {
            id: id.into(),
            name: id.into(),
            config_dir: root.join(id).to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn switching_saves_the_outgoing_login_and_installs_the_target() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        write_login(&live, "live@x.com", "LIVE-ROTATED");
        let global = root.path().join("global.json");
        std::fs::write(
            &global,
            r#"{"numStartups":3,"oauthAccount":{"emailAddress":"live@x.com"}}"#,
        )
        .unwrap();
        write_login(&root.path().join("work"), "work@x.com", "WORK-TOKEN");
        write_login(&root.path().join("home"), "live@x.com", "STALE-COPY");
        let mut accounts = crate::accounts::ProviderAccounts {
            profiles: vec![profile(root.path(), "work"), profile(root.path(), "home")],
            selected: "home".into(),
            ..Default::default()
        };
        let credentials = live.join(".credentials.json");

        let (name, already) = swap_files(&credentials, &global, &mut accounts, 0, |id| {
            root.path().join(id).to_string_lossy().into_owned()
        })
        .unwrap();
        assert_eq!((name.as_str(), already), ("work", false));
        // Target installed, outgoing (rotated) token saved over the stale copy.
        assert_eq!(std::fs::read_to_string(&credentials).unwrap(), "WORK-TOKEN");
        assert_eq!(
            std::fs::read_to_string(root.path().join("home/.credentials.json")).unwrap(),
            "LIVE-ROTATED"
        );
        assert_eq!(
            read_oauth_account(&global).unwrap()["emailAddress"],
            "work@x.com"
        );
        let json: Value = serde_json::from_slice(&std::fs::read(&global).unwrap()).unwrap();
        assert_eq!(json["numStartups"], 3);
        assert!(accounts.live_swap);
        assert_eq!(accounts.selected, "work");
        assert_eq!(accounts.profiles.len(), 2);
        assert!(live.join(".credentials.json.usage-monitor.bak").exists());

        // Switching to the already-live account changes nothing.
        let (_, already) = swap_files(&credentials, &global, &mut accounts, 0, |id| {
            root.path().join(id).to_string_lossy().into_owned()
        })
        .unwrap();
        assert!(already);
        assert_eq!(std::fs::read_to_string(&credentials).unwrap(), "WORK-TOKEN");

        // Round trip: the work token rotated while live and must be kept.
        std::fs::write(&credentials, "WORK-ROTATED").unwrap();
        swap_files(&credentials, &global, &mut accounts, 1, |id| {
            root.path().join(id).to_string_lossy().into_owned()
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&credentials).unwrap(),
            "LIVE-ROTATED"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("work/.credentials.json")).unwrap(),
            "WORK-ROTATED"
        );
    }

    #[test]
    fn an_unknown_live_login_is_saved_to_a_new_profile_before_it_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        write_login(&live, "stranger@x.com", "STRANGER");
        let global = root.path().join("global.json");
        std::fs::write(
            &global,
            r#"{"oauthAccount":{"emailAddress":"stranger@x.com"}}"#,
        )
        .unwrap();
        write_login(&root.path().join("work"), "work@x.com", "WORK");
        let mut accounts = crate::accounts::ProviderAccounts {
            profiles: vec![AccountProfile::default(), profile(root.path(), "work")],
            ..Default::default()
        };
        // The Default profile is the live account; it must stop duplicating it.
        swap_files(
            &live.join(".credentials.json"),
            &global,
            &mut accounts,
            1,
            |id| root.path().join(id).to_string_lossy().into_owned(),
        )
        .unwrap();
        let saved = accounts
            .profiles
            .iter()
            .find(|p| p.name == "stranger@x.com")
            .expect("outgoing login kept");
        let directory = profile_directory(saved).unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join(".credentials.json")).unwrap(),
            "STRANGER"
        );
        assert_eq!(profile_email(saved).as_deref(), Some("stranger@x.com"));
        assert!(!accounts.profiles[0].enabled);
    }

    #[test]
    fn switching_refuses_targets_it_cannot_identify_and_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        write_login(&live, "live@x.com", "LIVE");
        let global = root.path().join("global.json");
        std::fs::write(&global, r#"{"oauthAccount":{"emailAddress":"live@x.com"}}"#).unwrap();
        std::fs::create_dir_all(root.path().join("unsigned")).unwrap();
        let no_email = root.path().join("noemail");
        std::fs::create_dir_all(&no_email).unwrap();
        std::fs::write(no_email.join(".credentials.json"), "X").unwrap();
        let mut accounts = crate::accounts::ProviderAccounts {
            profiles: vec![
                profile(root.path(), "unsigned"),
                profile(root.path(), "noemail"),
                AccountProfile::default(),
            ],
            ..Default::default()
        };
        let credentials = live.join(".credentials.json");
        for index in 0..3 {
            assert!(
                swap_files(&credentials, &global, &mut accounts, index, |id| root
                    .path()
                    .join(id)
                    .to_string_lossy()
                    .into_owned())
                .is_err()
            );
        }
        assert!(
            swap_files(&credentials, &global, &mut accounts, 9, |id| root
                .path()
                .join(id)
                .to_string_lossy()
                .into_owned())
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&credentials).unwrap(), "LIVE");
        assert!(!accounts.live_swap);
    }

    #[test]
    fn reads_the_signed_in_email_and_refreshes_when_the_file_changes() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".claude.json");
        std::fs::write(
            &file,
            r#"{"oauthAccount":{"emailAddress":"a@example.com"}}"#,
        )
        .unwrap();
        assert_eq!(account_email(root.path()).as_deref(), Some("a@example.com"));
        std::fs::write(
            &file,
            r#"{"oauthAccount":{"emailAddress":"b@example.com"}, "padding": 1}"#,
        )
        .unwrap();
        assert_eq!(account_email(root.path()).as_deref(), Some("b@example.com"));
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(account_email(root.path()), None);
    }

    #[test]
    fn oauth_account_updates_keep_the_rest_of_the_config() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".claude.json");
        std::fs::write(
            &file,
            r#"{"numStartups": 7, "oauthAccount": {"emailAddress": "a@x.com"}}"#,
        )
        .unwrap();
        let account = serde_json::json!({"emailAddress": "b@x.com", "organizationName": "B"});
        write_oauth_account(&file, &account).unwrap();
        let json: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(json["numStartups"], 7);
        assert_eq!(json["oauthAccount"]["emailAddress"], "b@x.com");
        assert_eq!(read_oauth_account(&file).unwrap()["organizationName"], "B");
        // A damaged config is never overwritten.
        std::fs::write(&file, "{ broken").unwrap();
        assert!(write_oauth_account(&file, &account).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ broken");
        assert!(read_oauth_account(&root.path().join("missing.json")).is_none());
    }

    #[test]
    fn suggests_the_roomiest_other_account_only_near_a_limit() {
        use crate::models::{AccountUsage, UsageData, UsageSection};
        use crate::providers::ProviderId;
        let section = |percentage| UsageSection {
            available: true,
            percentage,
            ..Default::default()
        };
        let usage = |s, w| UsageData {
            session: section(s),
            weekly: section(w),
            ..Default::default()
        };
        let mk = |id: &str, dir: &str| AccountProfile {
            id: id.into(),
            name: id.into(),
            config_dir: dir.into(),
            ..Default::default()
        };
        let row = |p: &AccountProfile, u| AccountUsage {
            provider: ProviderId::Claude,
            profile: p.clone(),
            source_signature: String::new(),
            source_path: None,
            usage: Some(u),
            error: None,
            selected: false,
        };
        let live = AccountProfile::default();
        let a = mk("a", "C:\\acct\\a");
        let b = mk("b", "C:\\acct\\b");
        let profiles = vec![live.clone(), a.clone(), b.clone()];
        let rows = |l, x, y| vec![row(&live, l), row(&a, x), row(&b, y)];
        // The default profile is live only when no saved login matches it.
        assert!(suggest_switch(
            &profiles,
            &rows(usage(50.0, 40.0), usage(0.0, 0.0), usage(0.0, 0.0))
        )
        .is_none());
        let s = suggest_switch(
            &profiles,
            &rows(usage(95.0, 40.0), usage(10.0, 100.0), usage(5.0, 30.0)),
        )
        .unwrap();
        assert!(s.body.contains("switch to b"), "{}", s.body);
        let s = suggest_switch(
            &profiles,
            &rows(usage(10.0, 97.0), usage(0.0, 100.0), usage(95.0, 0.0)),
        )
        .unwrap();
        assert!(s.body.contains("No other tracked account"));
        let near = || {
            suggest_switch(
                &profiles,
                &rows(usage(95.0, 40.0), usage(0.0, 0.0), usage(0.0, 0.0)),
            )
        };
        assert!(announce_once(near()).is_some());
        assert!(announce_once(near()).is_none());
        assert!(announce_once(None).is_none());
        assert!(announce_once(near()).is_some());
    }

    #[test]
    fn reset_times_read_naturally() {
        use std::time::Duration;
        let at = |secs: u64| Some(SystemTime::now() + Duration::from_secs(secs + 30));
        assert_eq!(format_reset(at(25 * 60)).as_deref(), Some("25m"));
        assert_eq!(
            format_reset(at(2 * 3600 + 5 * 60)).as_deref(),
            Some("2h 5m")
        );
        assert_eq!(format_reset(at(30 * 3600)).as_deref(), Some("1d 6h"));
        assert_eq!(format_reset(None), None);
    }

    #[test]
    fn mcp_logins_follow_the_switch_and_the_newest_copy_wins() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        write_login(&live, "live@x.com", "ignored");
        let creds = |email_token: &str, mcp: Value| {
            serde_json::json!({"claudeAiOauth": {"accessToken": email_token}, "mcpOAuth": mcp})
                .to_string()
        };
        std::fs::write(
            live.join(".credentials.json"),
            creds("LIVE", serde_json::json!({"supabase": {"expiresAt": 300, "t": "live-new"}, "github": {"expiresAt": 10, "t": "gh"}})),
        )
        .unwrap();
        let global = root.path().join("global.json");
        std::fs::write(&global, r#"{"oauthAccount":{"emailAddress":"live@x.com"}}"#).unwrap();
        write_login(&root.path().join("work"), "work@x.com", "x");
        std::fs::write(
            root.path().join("work/.credentials.json"),
            creds(
                "WORK",
                serde_json::json!({"supabase": {"expiresAt": 100, "t": "work-old"}}),
            ),
        )
        .unwrap();
        let mut accounts = crate::accounts::ProviderAccounts {
            profiles: vec![profile(root.path(), "work")],
            ..Default::default()
        };
        let credentials = live.join(".credentials.json");
        swap_files(&credentials, &global, &mut accounts, 0, |id| {
            root.path().join(id).to_string_lossy().into_owned()
        })
        .unwrap();
        let installed: Value =
            serde_json::from_slice(&std::fs::read(&credentials).unwrap()).unwrap();
        assert_eq!(installed["claudeAiOauth"]["accessToken"], "WORK");
        assert_eq!(installed["mcpOAuth"]["supabase"]["t"], "live-new");
        assert_eq!(installed["mcpOAuth"]["github"]["t"], "gh");
    }

    /// Run with `CLAUDE_JSON_COPY=<path to a copy of ~/.claude.json>` to prove a
    /// switch leaves MCP servers and every other setting untouched.
    #[test]
    #[ignore]
    fn switching_leaves_a_real_claude_json_intact_apart_from_the_account() {
        let Some(source) = std::env::var_os("CLAUDE_JSON_COPY") else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let copy = root.path().join(".claude.json");
        std::fs::copy(source, &copy).unwrap();
        let before: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        let account = serde_json::json!({"emailAddress": "switched@example.com"});
        write_oauth_account(&copy, &account).unwrap();
        let mut after: Value = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        assert_eq!(after["oauthAccount"], account);
        let mut expected = before.clone();
        expected["oauthAccount"] = account;
        assert_eq!(after, expected);
        after["oauthAccount"] = before["oauthAccount"].clone();
        assert_eq!(after["mcpServers"], before["mcpServers"]);
        assert_eq!(after["projects"], before["projects"]);
    }

    #[test]
    fn placeholder_names_are_only_account_numbers() {
        assert!(is_placeholder_name("Account 12"));
        assert!(is_placeholder_name("Default"));
        // A blank non-default profile must not inherit the main login's email.
        let blank = AccountProfile {
            id: "account_2".into(),
            ..Default::default()
        };
        assert_eq!(profile_email(&blank), None);
        for name in ["Account", "Account ", "Account x", "Work", "a@b.com"] {
            assert!(!is_placeholder_name(name), "{name}");
        }
    }
}
