use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use freedesktop_desktop_entry::{DesktopEntry, Iter, default_paths};
use globset::{Glob, GlobSet, GlobSetBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{process::Command, task};
use x11_controller::{ControllerError, ErrorCode};

#[derive(Debug, Clone)]
pub struct ApplicationConfig {
    pub display: String,
    pub allow_apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApplicationCapabilities {
    pub desktop_entry_launch: bool,
    pub terminal_entries_excluded: bool,
    pub allowlist_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ListAppsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AppList {
    pub apps: Vec<AppInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchAppRequest {
    pub app_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LaunchAppResult {
    pub launched: bool,
    pub app_id: String,
    pub name: String,
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub struct ApplicationLauncher {
    display: Arc<str>,
    search_paths: Arc<[PathBuf]>,
    locales: Arc<[String]>,
    current_desktops: Arc<[String]>,
    executable_path: Option<Arc<OsString>>,
    allowlist: Arc<GlobSet>,
    allowlist_enabled: bool,
    emergency_stop: Arc<AtomicBool>,
}

#[derive(Debug)]
struct ApplicationRecord {
    info: AppInfo,
    entry: DesktopEntry,
    argv: Vec<String>,
}

impl ApplicationLauncher {
    pub fn new(
        config: ApplicationConfig,
        emergency_stop: Arc<AtomicBool>,
    ) -> Result<Self, ControllerError> {
        Self::with_environment(
            config,
            default_paths().collect(),
            locales_from_environment(),
            current_desktops_from_environment(),
            env::var_os("PATH"),
            emergency_stop,
        )
    }

    fn with_environment(
        config: ApplicationConfig,
        search_paths: Vec<PathBuf>,
        locales: Vec<String>,
        current_desktops: Vec<String>,
        executable_path: Option<OsString>,
        emergency_stop: Arc<AtomicBool>,
    ) -> Result<Self, ControllerError> {
        let allowlist_enabled = !config.allow_apps.is_empty();
        let mut builder = GlobSetBuilder::new();
        for pattern in config.allow_apps {
            let glob = Glob::new(&pattern).map_err(|error| {
                ControllerError::new(
                    ErrorCode::InvalidInput,
                    format!("invalid application allowlist glob {pattern:?}: {error}"),
                )
            })?;
            builder.add(glob);
        }
        let allowlist = builder.build().map_err(|error| {
            ControllerError::new(
                ErrorCode::InvalidInput,
                format!("build application allowlist: {error}"),
            )
        })?;
        Ok(Self {
            display: Arc::from(config.display),
            search_paths: search_paths.into(),
            locales: locales.into(),
            current_desktops: current_desktops.into(),
            executable_path: executable_path.map(Arc::new),
            allowlist: Arc::new(allowlist),
            allowlist_enabled,
            emergency_stop,
        })
    }

    #[must_use]
    pub fn capabilities(&self) -> ApplicationCapabilities {
        ApplicationCapabilities {
            desktop_entry_launch: true,
            terminal_entries_excluded: true,
            allowlist_enabled: self.allowlist_enabled,
        }
    }

    pub async fn list_apps(&self, request: ListAppsRequest) -> Result<AppList, ControllerError> {
        let launcher = self.clone();
        task::spawn_blocking(move || launcher.list_apps_sync(&request))
            .await
            .map_err(|error| join_error(&error))
    }

    fn list_apps_sync(&self, request: &ListAppsRequest) -> AppList {
        let query = request
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let mut apps = self
            .discover()
            .filter(|record| self.is_allowed(&record.info.app_id))
            .filter(|record| {
                query.as_ref().is_none_or(|query| {
                    record.info.app_id.to_lowercase().contains(query)
                        || record.info.name.to_lowercase().contains(query)
                })
            })
            .map(|record| record.info)
            .collect::<Vec<_>>();
        apps.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.app_id.cmp(&right.app_id))
        });
        AppList { apps }
    }

    pub async fn launch_app(
        &self,
        request: LaunchAppRequest,
    ) -> Result<LaunchAppResult, ControllerError> {
        self.ensure_mutation_allowed()?;
        let app_id = validate_app_id(&request.app_id)?;
        let launcher = self.clone();
        let lookup_id = app_id.clone();
        let record = task::spawn_blocking(move || launcher.resolve(&lookup_id))
            .await
            .map_err(|error| join_error(&error))??;
        self.ensure_mutation_allowed()?;
        self.spawn(record)
    }

    fn resolve(&self, app_id: &str) -> Result<ApplicationRecord, ControllerError> {
        let record = self
            .discover()
            .find(|record| record.info.app_id == app_id)
            .ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::InvalidInput,
                    format!("application is not available: {app_id}"),
                )
            })?;
        if !self.is_allowed(app_id) {
            return Err(ControllerError::new(
                ErrorCode::AccessDenied,
                format!("application is not allowed: {app_id}"),
            ));
        }
        Ok(record)
    }

    fn spawn(&self, record: ApplicationRecord) -> Result<LaunchAppResult, ControllerError> {
        let executable = record.argv.first().ok_or_else(|| {
            ControllerError::new(ErrorCode::Internal, "application command is empty")
        })?;
        let mut command = Command::new(executable);
        command
            .args(&record.argv[1..])
            .env("DISPLAY", self.display.as_ref())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(working_directory) = record.entry.path() {
            command.current_dir(working_directory);
        }
        let mut child = command.spawn().map_err(|error| {
            ControllerError::new(
                ErrorCode::Internal,
                format!(
                    "could not launch application {}: {error}",
                    record.info.app_id
                ),
            )
        })?;
        let pid = child.id().ok_or_else(|| {
            ControllerError::new(
                ErrorCode::Internal,
                format!(
                    "launched application {} without a process identifier",
                    record.info.app_id
                ),
            )
        })?;
        let result = LaunchAppResult {
            launched: true,
            app_id: record.info.app_id,
            name: record.info.name,
            pid,
        };
        let reaper_app_id = result.app_id.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    tracing::debug!(app_id = %reaper_app_id, %status, "application process exited");
                }
                Err(error) => {
                    tracing::warn!(app_id = %reaper_app_id, %error, "could not reap application process");
                }
            }
        });
        Ok(result)
    }

    fn discover(&self) -> impl Iterator<Item = ApplicationRecord> {
        let mut seen = HashSet::new();
        let mut records = Vec::new();
        for search_path in self.search_paths.iter() {
            let application_root = search_path
                .canonicalize()
                .unwrap_or_else(|_| search_path.clone());
            for path in Iter::new(std::iter::once(search_path.clone())) {
                let Some(app_id) = desktop_file_id(&application_root, &path) else {
                    continue;
                };
                if !seen.insert(app_id.clone()) {
                    continue;
                }
                let entry = match DesktopEntry::from_path(&path, Some(&self.locales)) {
                    Ok(entry) => entry,
                    Err(error) => {
                        tracing::debug!(%error, path = %path.display(), "ignoring invalid desktop entry");
                        continue;
                    }
                };
                if !self.is_visible(&entry) {
                    continue;
                }
                let Some(name) = entry.name(&self.locales).map(std::borrow::Cow::into_owned) else {
                    continue;
                };
                if name.trim().is_empty() || !self.entry_requirements_available(&entry) {
                    continue;
                }
                let argv = match parse_exec_argv(&entry, &self.locales) {
                    Ok(argv) if !argv.is_empty() => argv,
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::debug!(%error, app_id, "ignoring desktop entry with invalid Exec");
                        continue;
                    }
                };
                if !executable_available(&argv[0], self.executable_path.as_deref()) {
                    continue;
                }
                records.push(ApplicationRecord {
                    info: AppInfo { app_id, name },
                    entry,
                    argv,
                });
            }
        }
        records.into_iter()
    }

    fn is_visible(&self, entry: &DesktopEntry) -> bool {
        if entry.type_() != Some("Application")
            || entry.hidden()
            || entry.no_display()
            || entry.terminal()
            || entry.exec().is_none()
        {
            return false;
        }
        if entry.only_show_in().is_some_and(|desktops| {
            !desktops.iter().any(|desktop| {
                !desktop.is_empty()
                    && self
                        .current_desktops
                        .iter()
                        .any(|current| current == desktop)
            })
        }) {
            return false;
        }
        !entry.not_show_in().is_some_and(|desktops| {
            desktops.iter().any(|desktop| {
                !desktop.is_empty()
                    && self
                        .current_desktops
                        .iter()
                        .any(|current| current == desktop)
            })
        })
    }

    fn entry_requirements_available(&self, entry: &DesktopEntry) -> bool {
        entry.try_exec().is_none_or(|executable| {
            executable_available(executable, self.executable_path.as_deref())
        }) && entry.path().is_none_or(|path| Path::new(path).is_dir())
    }

    fn is_allowed(&self, app_id: &str) -> bool {
        !self.allowlist_enabled || self.allowlist.is_match(app_id)
    }

    fn ensure_mutation_allowed(&self) -> Result<(), ControllerError> {
        if self.emergency_stop.load(Ordering::SeqCst) {
            Err(ControllerError::new(
                ErrorCode::EmergencyStop,
                "application launch is disabled because the emergency stop is latched",
            ))
        } else {
            Ok(())
        }
    }
}

fn validate_app_id(app_id: &str) -> Result<String, ControllerError> {
    if app_id.is_empty()
        || app_id != app_id.trim()
        || app_id.len() > 512
        || app_id.contains(['/', '\0'])
    {
        Err(ControllerError::new(
            ErrorCode::InvalidInput,
            "app_id must be an exact non-empty desktop-entry identifier without surrounding whitespace or path separators",
        ))
    } else {
        Ok(app_id.to_owned())
    }
}

fn desktop_file_id(application_root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(application_root).ok()?;
    let value = relative.to_str()?.strip_suffix(".desktop")?;
    (!value.is_empty()).then(|| value.replace('/', "-"))
}

fn executable_available(executable: &str, path: Option<&OsString>) -> bool {
    let executable_path = Path::new(executable);
    if executable_path.is_absolute() {
        return is_executable(executable_path);
    }
    if executable.contains('/') || executable.is_empty() {
        return false;
    }
    path.is_some_and(|path| {
        env::split_paths(path).any(|directory| is_executable(&directory.join(executable)))
    })
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn parse_exec_argv(
    entry: &DesktopEntry,
    locales: &[String],
) -> Result<Vec<String>, ControllerError> {
    let exec = entry.exec().ok_or_else(|| {
        ControllerError::new(ErrorCode::InvalidInput, "desktop entry has no Exec value")
    })?;
    entry.parse_exec().map_err(|error| {
        ControllerError::new(
            ErrorCode::InvalidInput,
            format!("desktop entry Exec parser rejected the value: {error}"),
        )
    })?;
    // The crate parser establishes baseline compatibility; the strict tokenizer below remains
    // authoritative for freedesktop quoting and standard no-input field-code expansion.
    let tokens = tokenize_exec(exec)?;
    let mut argv = Vec::with_capacity(tokens.len());
    for token in tokens {
        argv.extend(expand_exec_token(&token, entry, locales)?);
    }
    if argv.is_empty() || argv[0].contains('=') {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            "desktop entry Exec value has no valid executable",
        ));
    }
    Ok(argv)
}

fn tokenize_exec(exec: &str) -> Result<Vec<String>, ControllerError> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut token_started = false;
    let mut quoted = false;
    let mut characters = exec.chars();
    while let Some(character) = characters.next() {
        if quoted {
            match character {
                '"' => quoted = false,
                '\\' => {
                    let escaped = characters.next().ok_or_else(exec_format_error)?;
                    if matches!(escaped, '"' | '`' | '$' | '\\') {
                        token.push(escaped);
                    } else {
                        return Err(exec_format_error());
                    }
                }
                _ => token.push(character),
            }
            token_started = true;
            continue;
        }
        match character {
            '"' => {
                quoted = true;
                token_started = true;
            }
            character if character.is_whitespace() => {
                if token_started {
                    tokens.push(std::mem::take(&mut token));
                    token_started = false;
                }
            }
            '\\' | '\'' | '>' | '<' | '~' | '|' | '&' | ';' | '$' | '*' | '?' | '#' | '(' | ')'
            | '`' => return Err(exec_format_error()),
            _ => {
                token.push(character);
                token_started = true;
            }
        }
    }
    if quoted {
        return Err(exec_format_error());
    }
    if token_started {
        tokens.push(token);
    }
    Ok(tokens)
}

fn expand_exec_token(
    token: &str,
    entry: &DesktopEntry,
    locales: &[String],
) -> Result<Vec<String>, ControllerError> {
    if matches!(
        token,
        "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m"
    ) {
        return Ok(Vec::new());
    }
    if token == "%i" {
        return Ok(entry
            .icon()
            .map_or_else(Vec::new, |icon| vec!["--icon".to_owned(), icon.to_owned()]));
    }
    let mut expanded = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            expanded.push(character);
            continue;
        }
        let code = characters.next().ok_or_else(exec_format_error)?;
        match code {
            '%' => expanded.push('%'),
            'c' => {
                if let Some(name) = entry.name(locales) {
                    expanded.push_str(&name);
                }
            }
            'k' => expanded.push_str(&entry.path.to_string_lossy()),
            'f' | 'F' | 'u' | 'U' | 'i' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {
                return Err(exec_format_error());
            }
            _ => return Err(exec_format_error()),
        }
    }
    Ok(vec![expanded])
}

fn exec_format_error() -> ControllerError {
    ControllerError::new(
        ErrorCode::InvalidInput,
        "desktop entry Exec value has invalid quoting or field codes",
    )
}

fn locales_from_environment() -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|key| env::var(key).ok().filter(|value| !value.is_empty()));
    locale.map_or_else(Vec::new, |locale| locale_candidates(&locale))
}

fn locale_candidates(locale: &str) -> Vec<String> {
    let (locale_without_modifier, modifier) = locale.split_once('@').unwrap_or((locale, ""));
    let base = locale_without_modifier
        .split('.')
        .next()
        .unwrap_or(locale_without_modifier);
    if matches!(base, "C" | "POSIX") {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    if !modifier.is_empty() {
        candidates.push(format!("{base}@{modifier}"));
    }
    if !candidates.iter().any(|candidate| candidate == base) {
        candidates.push(base.to_owned());
    }
    if let Some((language, _country)) = base.split_once('_') {
        if !modifier.is_empty() {
            candidates.push(format!("{language}@{modifier}"));
        }
        candidates.push(language.to_owned());
    }
    candidates.dedup();
    candidates
}

fn current_desktops_from_environment() -> Vec<String> {
    env::var("XDG_CURRENT_DESKTOP").map_or_else(
        |_| Vec::new(),
        |value| {
            value
                .split(':')
                .filter(|desktop| !desktop.is_empty())
                .map(str::to_owned)
                .collect()
        },
    )
}

fn join_error(error: &task::JoinError) -> ControllerError {
    ControllerError::new(
        ErrorCode::Internal,
        format!("application discovery task failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use tempfile::TempDir;

    use super::*;

    fn launcher(
        display: &str,
        roots: Vec<PathBuf>,
        allow_apps: Vec<String>,
        desktops: Vec<String>,
        emergency_stop: Arc<AtomicBool>,
    ) -> ApplicationLauncher {
        ApplicationLauncher::with_environment(
            ApplicationConfig {
                display: display.to_owned(),
                allow_apps,
            },
            roots,
            vec!["fr_CA".to_owned(), "fr".to_owned()],
            desktops,
            env::var_os("PATH"),
            emergency_stop,
        )
        .unwrap()
    }

    fn write_entry(root: &Path, relative: &str, body: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn entry(name: &str, extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec=true\n{extra}")
    }

    #[test]
    fn discovers_visible_localized_apps_with_xdg_precedence() {
        let high = TempDir::new().unwrap();
        let low = TempDir::new().unwrap();
        let high_apps = high.path().join("applications");
        let low_apps = low.path().join("applications");
        write_entry(&low_apps, "shared.desktop", &entry("Lower priority", ""));
        write_entry(
            &high_apps,
            "shared.desktop",
            &entry("Hidden override", "Hidden=true\n"),
        );
        write_entry(
            &high_apps,
            "localized.desktop",
            "[Desktop Entry]\nType=Application\nName=English\nName[fr]=Français\nExec=true\nOnlyShowIn=XFCE;\n",
        );
        write_entry(&high_apps, "nested/tool.desktop", &entry("Nested Tool", ""));
        write_entry(
            &high_apps,
            "no-display.desktop",
            &entry("No Display", "NoDisplay=true\n"),
        );
        write_entry(
            &high_apps,
            "terminal.desktop",
            &entry("Terminal", "Terminal=true\n"),
        );
        write_entry(
            &high_apps,
            "wrong-desktop.desktop",
            &entry("Wrong Desktop", "OnlyShowIn=GNOME;\n"),
        );
        write_entry(
            &high_apps,
            "not-here.desktop",
            &entry("Not Here", "NotShowIn=XFCE;\n"),
        );
        write_entry(
            &high_apps,
            "missing.desktop",
            &entry("Missing", "TryExec=x11-mcp-definitely-missing\n"),
        );
        write_entry(
            &high_apps,
            "available.desktop",
            &entry("Available", "TryExec=true\n"),
        );
        write_entry(&high_apps, "malformed.desktop", "not a desktop entry");

        let launcher = launcher(
            ":71",
            vec![high_apps, low_apps],
            Vec::new(),
            vec!["XFCE".to_owned()],
            Arc::new(AtomicBool::new(false)),
        );
        let apps = launcher.list_apps_sync(&ListAppsRequest::default()).apps;
        assert_eq!(
            apps,
            vec![
                AppInfo {
                    app_id: "available".to_owned(),
                    name: "Available".to_owned(),
                },
                AppInfo {
                    app_id: "localized".to_owned(),
                    name: "Français".to_owned(),
                },
                AppInfo {
                    app_id: "nested-tool".to_owned(),
                    name: "Nested Tool".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn sorts_by_case_insensitive_localized_name_then_id() {
        let root = TempDir::new().unwrap();
        let apps = root.path().join("applications");
        write_entry(&apps, "z.desktop", &entry("alpha", ""));
        write_entry(&apps, "a.desktop", &entry("Alpha", ""));
        let launcher = launcher(
            ":72",
            vec![apps],
            Vec::new(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(
            launcher.list_apps_sync(&ListAppsRequest::default()).apps,
            vec![
                AppInfo {
                    app_id: "a".to_owned(),
                    name: "Alpha".to_owned(),
                },
                AppInfo {
                    app_id: "z".to_owned(),
                    name: "alpha".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn validates_exact_desktop_entry_ids() {
        assert!(validate_app_id("org.example.App").is_ok());
        assert!(validate_app_id(" org.example.App").is_err());
        assert!(validate_app_id("org/example/App").is_err());
    }

    #[tokio::test]
    async fn searches_and_filters_with_allowlist() {
        let root = TempDir::new().unwrap();
        let apps = root.path().join("applications");
        write_entry(
            &apps,
            "org.example.Editor.desktop",
            &entry("Text Editor", ""),
        );
        write_entry(
            &apps,
            "org.example.Viewer.desktop",
            &entry("Image Viewer", ""),
        );
        let launcher = launcher(
            ":72",
            vec![apps],
            vec!["org.example.E*".to_owned()],
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(launcher.capabilities().allowlist_enabled);
        let listed = launcher.list_apps_sync(&ListAppsRequest {
            query: Some("editor".to_owned()),
        });
        assert_eq!(listed.apps.len(), 1);
        assert_eq!(listed.apps[0].app_id, "org.example.Editor");
        let denied = launcher
            .launch_app(LaunchAppRequest {
                app_id: "org.example.Viewer".to_owned(),
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::AccessDenied);
    }

    #[test]
    fn expands_standard_exec_fields_without_shell_parsing() {
        let root = TempDir::new().unwrap();
        let path = write_entry(
            root.path(),
            "fields.desktop",
            "[Desktop Entry]\nType=Application\nName=Localized Name\nIcon=sample-icon\nExec=/usr/bin/printf \"hello world\" %c %i %% %f %U\n",
        );
        let locales = vec!["en".to_owned()];
        let desktop_entry = DesktopEntry::from_path(&path, Some(&locales)).unwrap();
        assert_eq!(
            parse_exec_argv(&desktop_entry, &locales).unwrap(),
            vec![
                "/usr/bin/printf",
                "hello world",
                "Localized Name",
                "--icon",
                "sample-icon",
                "%",
            ]
        );

        let unsafe_path = write_entry(
            root.path(),
            "unsafe.desktop",
            "[Desktop Entry]\nType=Application\nName=Unsafe\nExec=true; touch /tmp/not-run\n",
        );
        let unsafe_entry = DesktopEntry::from_path(&unsafe_path, Some(&locales)).unwrap();
        assert!(parse_exec_argv(&unsafe_entry, &locales).is_err());
    }

    #[test]
    fn rejects_invalid_allowlist_patterns() {
        let result = ApplicationLauncher::with_environment(
            ApplicationConfig {
                display: ":73".to_owned(),
                allow_apps: vec!["[".to_owned()],
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
            env::var_os("PATH"),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(result.unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn launch_uses_selected_display_and_working_directory() {
        let root = TempDir::new().unwrap();
        let apps = root.path().join("applications");
        let working_directory = root.path().join("working");
        fs::create_dir(&working_directory).unwrap();
        let executable = root.path().join("capture-environment");
        let display_output = root.path().join("display-output");
        let directory_output = root.path().join("directory-output");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' \"$DISPLAY\" > \"$1\"\npwd > \"$2\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        write_entry(
            &apps,
            "capture.desktop",
            &format!(
                "[Desktop Entry]\nType=Application\nName=Capture\nExec={} {} {}\nPath={}\nDBusActivatable=true\n",
                executable.display(),
                display_output.display(),
                directory_output.display(),
                working_directory.display()
            ),
        );
        let launcher = launcher(
            ":74",
            vec![apps],
            Vec::new(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        );

        let launch_result = launcher
            .launch_app(LaunchAppRequest {
                app_id: "capture".to_owned(),
            })
            .await
            .unwrap();
        assert!(launch_result.launched);
        assert!(launch_result.pid > 0);
        for _ in 0..100 {
            if display_output.exists() && directory_output.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fs::read_to_string(display_output).unwrap(), ":74");
        assert_eq!(
            fs::read_to_string(directory_output).unwrap().trim(),
            working_directory.to_string_lossy()
        );
    }

    #[tokio::test]
    async fn reports_spawn_failures_after_resolution() {
        let root = TempDir::new().unwrap();
        let apps = root.path().join("applications");
        let executable = root.path().join("disappearing-app");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        write_entry(
            &apps,
            "disappearing.desktop",
            &format!(
                "[Desktop Entry]\nType=Application\nName=Disappearing\nExec={}\n",
                executable.display()
            ),
        );
        let launcher = launcher(
            ":75",
            vec![apps],
            Vec::new(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        );
        let record = launcher.resolve("disappearing").unwrap();
        fs::remove_file(executable).unwrap();
        let error = launcher.spawn(record).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[tokio::test]
    async fn emergency_stop_rejects_launch_before_discovery() {
        let emergency_stop = Arc::new(AtomicBool::new(true));
        let launcher = launcher(":76", Vec::new(), Vec::new(), Vec::new(), emergency_stop);
        let error = launcher
            .launch_app(LaunchAppRequest {
                app_id: "anything".to_owned(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::EmergencyStop);
    }

    #[test]
    fn builds_freedesktop_locale_fallbacks() {
        assert_eq!(
            locale_candidates("sr_RS.UTF-8@latin"),
            vec!["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(locale_candidates("fr_CA.UTF-8"), vec!["fr_CA", "fr"]);
        assert!(locale_candidates("C").is_empty());
    }
}
