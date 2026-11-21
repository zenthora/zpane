use super::catalog::{
    cache_dir_from, list_spec_files_in_dir, non_empty_abs_env_path, read_bounded_bytes,
    ShellDialect, MAX_SPEC_FILES,
};
use super::config::{config_path_from, load_user_config_from_path_with_diagnostics};
use super::context::detect_git_context_in;
use super::history::detect_shell_history_with_source_from;
use super::introspect::{
    cli_cache_path_in, count_cli_cache_entries_from_bytes, find_executable_in_path,
    MAX_CLI_CACHE_BYTES,
};
use super::powershell::{count_ps_metadata_commands, MAX_PS_METADATA_BYTES};
use super::project_tasks::detect_project_task_files;
use super::specs::{parse_spec_str, spec_search_dirs_from, SpecDirEnv};
use crate::sanitize::strip_escapes_and_controls_into;
use serde::Serialize;
use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_HISTORY_LIMIT: usize = 15;

/// Injected system-boundary environment snapshot for deterministic, read-only diagnostics.
#[derive(Debug, Clone)]
pub struct DoctorEnv {
    pub is_windows: bool,
    pub is_macos: bool,
    pub zpane_shell: Option<String>,
    pub ps_module_path_present: bool,
    pub fish_version_present: bool,
    pub nu_version_present: bool,
    pub shell: Option<String>,
    pub zpane_config: Option<String>,
    pub histfile: Option<String>,
    pub no_color: Option<String>,
    pub term: Option<String>,
    pub home: Option<PathBuf>,
    pub userprofile: Option<PathBuf>,
    pub appdata: Option<PathBuf>,
    pub local_appdata: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub now: SystemTime,
}

impl DoctorEnv {
    /// Captures the active process environment and current working directory without mutating disk.
    pub fn from_process_env() -> Self {
        let no_color =
            env::var_os("NO_COLOR").and_then(|v| (!v.is_empty()).then(|| "1".to_string()));
        Self {
            is_windows: cfg!(windows),
            is_macos: cfg!(target_os = "macos"),
            zpane_shell: env::var("ZPANE_SHELL").ok(),
            ps_module_path_present: env::var_os("PSModulePath").is_some(),
            fish_version_present: env::var_os("FISH_VERSION").is_some(),
            nu_version_present: env::var_os("NU_VERSION").is_some(),
            shell: env::var("SHELL").ok(),
            zpane_config: env::var("ZPANE_CONFIG").ok(),
            histfile: env::var("HISTFILE").ok(),
            no_color,
            term: env::var("TERM").ok(),
            home: non_empty_abs_env_path("HOME"),
            userprofile: non_empty_abs_env_path("USERPROFILE"),
            appdata: non_empty_abs_env_path("APPDATA"),
            local_appdata: non_empty_abs_env_path("LOCALAPPDATA"),
            xdg_config_home: non_empty_abs_env_path("XDG_CONFIG_HOME"),
            xdg_cache_home: non_empty_abs_env_path("XDG_CACHE_HOME"),
            xdg_data_home: non_empty_abs_env_path("XDG_DATA_HOME"),
            cwd: env::current_dir().ok(),
            now: SystemTime::now(),
        }
    }
}

/// Complete read-only diagnostic report produced by `zpane doctor [--json]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub dialect: DialectDiagnostics,
    pub config: ConfigDiagnostics,
    pub cache: CacheDiagnostics,
    pub specs: SpecsDiagnostics,
    pub providers: ProvidersDiagnostics,
    pub runtime: RuntimeDiagnostics,
}

/// Detected [`ShellDialect`] and the environment rule that selected it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DialectDiagnostics {
    pub dialect: String,
    pub label: String,
    pub reason: String,
}

/// Non-fatal `config.toml` diagnostic entry suitable for JSON and human output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigDiagnosticEntry {
    pub line: usize,
    pub message: String,
}

/// Effective `[ui]` configuration and `NO_COLOR` / `TERM=dumb` resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UiDiagnostics {
    pub configured_theme: String,
    pub effective_theme: String,
    pub no_color: bool,
    pub dumb_term: bool,
    pub icons: String,
    pub borders: String,
    pub height: Option<String>,
    pub min_height: Option<usize>,
    pub effective_min_height: usize,
}

/// Active overrides resolved from `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigOverrides {
    pub priority_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub history_limit: Option<usize>,
    pub effective_history_limit: usize,
    pub introspect_timeout_ms: Option<u64>,
    pub effective_introspect_timeout_ms: u64,
    pub sync_profile: bool,
    pub ui: UiDiagnostics,
    pub keybindings_count: usize,
}

/// Resolved `config.toml` location, parse status, diagnostics, and active overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigDiagnostics {
    pub path: String,
    pub exists: bool,
    pub status: String,
    pub diagnostics: Vec<ConfigDiagnosticEntry>,
    pub overrides: ConfigOverrides,
}

/// Read-only inspection status of `ps_metadata.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PsMetadataCacheDiagnostics {
    pub path: String,
    pub exists: bool,
    pub status: String,
    pub command_count: Option<usize>,
    pub age_seconds: Option<u64>,
}

/// Read-only inspection status of `cli_cache_<dialect>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliCacheDiagnostics {
    pub path: String,
    pub exists: bool,
    pub status: String,
    pub entry_count: Option<usize>,
    pub age_seconds: Option<u64>,
}

/// Cache directory and persisted metadata cache file diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheDiagnostics {
    pub dir: String,
    pub dir_exists: bool,
    pub ps_metadata: PsMetadataCacheDiagnostics,
    pub cli_cache: CliCacheDiagnostics,
}

/// Single declarative spec directory entry in precedence order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpecDirDiagnostics {
    pub path: String,
    pub exists: bool,
    pub valid_files: Vec<String>,
}

/// Single valid declarative spec file discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadedSpecDiagnostics {
    pub file: String,
    pub path: String,
    pub command: String,
}

/// Declarative spec search directories and discovered valid spec files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpecsDiagnostics {
    pub search_dirs: Vec<SpecDirDiagnostics>,
    pub loaded_files: Vec<LoadedSpecDiagnostics>,
}

/// Availability and resolved binary path of one external provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderAvailability {
    pub available: bool,
    pub path: Option<String>,
}

/// Availability of external discovery providers (`pwsh`, `powershell.exe`, `carapace`, `man`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvidersDiagnostics {
    pub pwsh: ProviderAvailability,
    pub powershell_exe: ProviderAvailability,
    pub carapace: ProviderAvailability,
    pub man: ProviderAvailability,
}

/// Shell history source file and detected entry count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryDiagnostics {
    pub source_path: Option<String>,
    pub entry_count: usize,
}

/// Project-local task file detected in the current working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectTaskFileDiagnostics {
    pub file: String,
    pub path: String,
    pub task_count: usize,
}

/// Runtime context detected for the active directory and shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeDiagnostics {
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub history: HistoryDiagnostics,
    pub project_task_files: Vec<ProjectTaskFileDiagnostics>,
}

fn sanitize_text(raw: &str) -> String {
    let mut out = String::new();
    strip_escapes_and_controls_into(raw, &mut out);
    out
}

fn sanitize_path(path: &Path) -> String {
    sanitize_text(&path.to_string_lossy())
}

fn file_age_seconds(meta: &fs::Metadata, now: SystemTime) -> Option<u64> {
    let modified = meta.modified().ok()?;
    Some(now.duration_since(modified).unwrap_or_default().as_secs())
}

fn inspect_ps_metadata_cache(path: &Path, now: SystemTime) -> PsMetadataCacheDiagnostics {
    let clean_path = sanitize_path(path);
    match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => PsMetadataCacheDiagnostics {
            path: clean_path,
            exists: false,
            status: "missing".to_string(),
            command_count: None,
            age_seconds: None,
        },
        Err(_) => PsMetadataCacheDiagnostics {
            path: clean_path,
            exists: true,
            status: "unreadable".to_string(),
            command_count: None,
            age_seconds: None,
        },
        Ok(meta) => {
            let age_seconds = file_age_seconds(&meta, now);
            if !meta.file_type().is_file() {
                return PsMetadataCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "unreadable".to_string(),
                    command_count: None,
                    age_seconds,
                };
            }
            if meta.len() > MAX_PS_METADATA_BYTES {
                return PsMetadataCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "oversized".to_string(),
                    command_count: None,
                    age_seconds,
                };
            }
            let Some(bytes) = read_bounded_bytes(path, MAX_PS_METADATA_BYTES) else {
                return PsMetadataCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "unreadable".to_string(),
                    command_count: None,
                    age_seconds,
                };
            };
            match count_ps_metadata_commands(&bytes) {
                Ok(count) => PsMetadataCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "ok".to_string(),
                    command_count: Some(count),
                    age_seconds,
                },
                Err(()) => PsMetadataCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "invalid".to_string(),
                    command_count: None,
                    age_seconds,
                },
            }
        }
    }
}

fn inspect_cli_cache(path: &Path, dialect: ShellDialect, now: SystemTime) -> CliCacheDiagnostics {
    let clean_path = sanitize_path(path);
    match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => CliCacheDiagnostics {
            path: clean_path,
            exists: false,
            status: "missing".to_string(),
            entry_count: None,
            age_seconds: None,
        },
        Err(_) => CliCacheDiagnostics {
            path: clean_path,
            exists: true,
            status: "unreadable".to_string(),
            entry_count: None,
            age_seconds: None,
        },
        Ok(meta) => {
            let age_seconds = file_age_seconds(&meta, now);
            if !meta.file_type().is_file() {
                return CliCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "unreadable".to_string(),
                    entry_count: None,
                    age_seconds,
                };
            }
            if meta.len() > MAX_CLI_CACHE_BYTES {
                return CliCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "oversized".to_string(),
                    entry_count: None,
                    age_seconds,
                };
            }
            let Some(bytes) = read_bounded_bytes(path, MAX_CLI_CACHE_BYTES) else {
                return CliCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "unreadable".to_string(),
                    entry_count: None,
                    age_seconds,
                };
            };
            match count_cli_cache_entries_from_bytes(&bytes, dialect) {
                Ok(count) => CliCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "ok".to_string(),
                    entry_count: Some(count),
                    age_seconds,
                },
                Err(()) => CliCacheDiagnostics {
                    path: clean_path,
                    exists: true,
                    status: "invalid".to_string(),
                    entry_count: None,
                    age_seconds,
                },
            }
        }
    }
}

fn inspect_specs(search_dirs: &[PathBuf]) -> SpecsDiagnostics {
    let mut dir_reports = Vec::with_capacity(search_dirs.len());
    let mut loaded_files = Vec::new();
    let mut seen_canon_dirs = HashSet::new();
    let mut total_scanned = 0usize;

    for dir in search_dirs {
        let exists = fs::metadata(dir).is_ok_and(|m| m.is_dir());
        let mut valid_files = Vec::new();

        let remaining = MAX_SPEC_FILES.saturating_sub(total_scanned);
        if exists && remaining > 0 {
            let candidates = list_spec_files_in_dir(dir, remaining, &mut seen_canon_dirs);
            total_scanned = total_scanned.saturating_add(candidates.len());
            for (name, path, body) in candidates {
                if let Ok(entry) = parse_spec_str(&name, &body) {
                    let clean_file = sanitize_text(&name);
                    let clean_path = sanitize_path(&path);
                    let clean_cmd = sanitize_text(&entry.text);
                    valid_files.push(clean_file.clone());
                    loaded_files.push(LoadedSpecDiagnostics {
                        file: clean_file,
                        path: clean_path,
                        command: clean_cmd,
                    });
                }
            }
        }

        dir_reports.push(SpecDirDiagnostics {
            path: sanitize_path(dir),
            exists,
            valid_files,
        });
    }

    SpecsDiagnostics {
        search_dirs: dir_reports,
        loaded_files,
    }
}

fn probe_provider<F>(find_exe: &mut F, name: &str) -> ProviderAvailability
where
    F: FnMut(&str) -> Option<PathBuf>,
{
    match find_exe(name) {
        Some(path) => ProviderAvailability {
            available: true,
            path: Some(sanitize_path(&path)),
        },
        None => ProviderAvailability {
            available: false,
            path: None,
        },
    }
}

/// Builds a strictly read-only [`DoctorReport`] from an injected [`DoctorEnv`] and executable probe.
/// Never creates or mutates cache directories, config files, or metadata files.
pub fn build_doctor_report<F>(env: &DoctorEnv, mut find_exe: F) -> DoctorReport
where
    F: FnMut(&str) -> Option<PathBuf>,
{
    let (dialect, reason) = ShellDialect::detect_with_reason_from_env(
        env.zpane_shell.as_deref(),
        env.ps_module_path_present,
        env.fish_version_present,
        env.nu_version_present,
        env.shell.as_deref(),
        env.is_windows,
    );

    let cache_dir = cache_dir_from(
        env.is_windows,
        env.local_appdata.as_deref(),
        env.userprofile.as_deref(),
        env.xdg_cache_home.as_deref(),
        env.home.as_deref(),
    );

    let cfg_path = config_path_from(
        env.is_windows,
        env.zpane_config.as_deref(),
        env.local_appdata.as_deref(),
        env.userprofile.as_deref(),
        env.xdg_config_home.as_deref(),
        env.home.as_deref(),
        &cache_dir,
    );

    let cfg_meta = fs::metadata(&cfg_path);
    let cfg_exists = !matches!(&cfg_meta, Err(e) if e.kind() == io::ErrorKind::NotFound);
    let (user_cfg, raw_diags) = load_user_config_from_path_with_diagnostics(&cfg_path);
    let cfg_status = if !cfg_exists {
        "missing"
    } else if raw_diags.is_empty() {
        "ok"
    } else if raw_diags.iter().any(|d| d.line == 0) {
        "error"
    } else {
        "warnings"
    };

    let no_color_active = env.no_color.as_deref().is_some_and(|v| !v.is_empty());
    let dumb_term = env
        .term
        .as_deref()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("dumb"));
    let resolved_ui = user_cfg
        .ui
        .resolve_env(env.no_color.as_deref(), env.term.as_deref());
    let effective_history_limit = user_cfg.history_limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    let effective_timeout_ms = user_cfg.effective_introspect_timeout().as_millis() as u64;

    let config = ConfigDiagnostics {
        path: sanitize_path(&cfg_path),
        exists: cfg_exists,
        status: cfg_status.to_string(),
        diagnostics: raw_diags
            .into_iter()
            .map(|d| ConfigDiagnosticEntry {
                line: d.line,
                message: sanitize_text(&d.message),
            })
            .collect(),
        overrides: ConfigOverrides {
            priority_tools: user_cfg
                .priority_tools
                .iter()
                .map(|t| sanitize_text(t))
                .collect(),
            disabled_tools: user_cfg
                .disabled_tools
                .iter()
                .map(|t| sanitize_text(t))
                .collect(),
            history_limit: user_cfg.history_limit,
            effective_history_limit,
            introspect_timeout_ms: user_cfg.introspect_timeout_ms,
            effective_introspect_timeout_ms: effective_timeout_ms,
            sync_profile: user_cfg.sync_profile,
            ui: UiDiagnostics {
                configured_theme: user_cfg.ui.theme.as_str().to_string(),
                effective_theme: resolved_ui.theme.as_str().to_string(),
                no_color: no_color_active,
                dumb_term,
                icons: resolved_ui.icons.as_str().to_string(),
                borders: resolved_ui.borders.as_str().to_string(),
                height: user_cfg.ui.height.map(|h| h.to_string()),
                min_height: user_cfg.ui.min_height,
                effective_min_height: user_cfg.ui.effective_min_height(),
            },
            keybindings_count: user_cfg.keybindings.bindings.len(),
        },
    };

    let ps_meta_path = cache_dir.join("ps_metadata.json");
    let cli_cache_path = cli_cache_path_in(&cache_dir, dialect);
    let cache = CacheDiagnostics {
        dir: sanitize_path(&cache_dir),
        dir_exists: fs::metadata(&cache_dir).is_ok_and(|m| m.is_dir()),
        ps_metadata: inspect_ps_metadata_cache(&ps_meta_path, env.now),
        cli_cache: inspect_cli_cache(&cli_cache_path, dialect, env.now),
    };

    let spec_dirs = spec_search_dirs_from(&SpecDirEnv {
        is_windows: env.is_windows,
        xdg_config_home: env.xdg_config_home.clone(),
        home: env.home.clone(),
        appdata: env.appdata.clone(),
        local_appdata: env.local_appdata.clone(),
        userprofile: env.userprofile.clone(),
        cache_dir: cache_dir.clone(),
    });
    let specs = inspect_specs(&spec_dirs);

    let providers = ProvidersDiagnostics {
        pwsh: probe_provider(&mut find_exe, "pwsh"),
        powershell_exe: probe_provider(&mut find_exe, "powershell.exe"),
        carapace: probe_provider(&mut find_exe, "carapace"),
        man: probe_provider(&mut find_exe, "man"),
    };

    let effective_home = env.home.as_deref().or(env.userprofile.as_deref());
    let (hist_source, hist_entries) = detect_shell_history_with_source_from(
        dialect,
        effective_history_limit,
        env.is_windows,
        env.is_macos,
        env.histfile.as_deref(),
        env.shell.as_deref(),
        env.appdata.as_deref(),
        env.xdg_data_home.as_deref(),
        env.xdg_config_home.as_deref(),
        effective_home,
    );

    let (git_branch, project_task_files) = match env.cwd.as_deref() {
        Some(cwd) => {
            let (branch, _) = detect_git_context_in(cwd);
            let tasks = detect_project_task_files(cwd)
                .into_iter()
                .map(|(file, path, task_count)| ProjectTaskFileDiagnostics {
                    file: sanitize_text(&file),
                    path: sanitize_path(&path),
                    task_count,
                })
                .collect();
            (branch.map(|b| sanitize_text(&b)), tasks)
        }
        None => (None, Vec::new()),
    };

    let runtime = RuntimeDiagnostics {
        cwd: env.cwd.as_deref().map(sanitize_path),
        git_branch,
        history: HistoryDiagnostics {
            source_path: hist_source.as_deref().map(sanitize_path),
            entry_count: hist_entries.len(),
        },
        project_task_files,
    };

    DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        dialect: DialectDiagnostics {
            dialect: dialect.tag().to_string(),
            label: dialect.display_name().to_string(),
            reason: sanitize_text(&reason),
        },
        config,
        cache,
        specs,
        providers,
        runtime,
    }
}

/// Collects a strictly read-only [`DoctorReport`] for the active process environment.
pub fn collect_doctor_report() -> DoctorReport {
    let env = DoctorEnv::from_process_env();
    build_doctor_report(&env, find_executable_in_path)
}

fn format_provider_line(out: &mut String, name: &str, p: &ProviderAvailability) {
    if p.available {
        let path = p
            .path
            .as_deref()
            .map(sanitize_text)
            .unwrap_or_else(|| "found".to_string());
        let _ = writeln!(out, "  {name:<14}: available ({path})");
    } else {
        let _ = writeln!(out, "  {name:<14}: missing");
    }
}

/// Formats [`DoctorReport`] as a clean, sanitized plain-text report for `zpane doctor`.
pub fn format_doctor_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "zpane doctor (v{})", sanitize_text(&report.version));
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "Shell Dialect: {} ({})",
        sanitize_text(&report.dialect.label),
        sanitize_text(&report.dialect.dialect)
    );
    let _ = writeln!(
        out,
        "  Reason     : {}",
        sanitize_text(&report.dialect.reason)
    );
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "Config: {} [{}]",
        sanitize_text(&report.config.path),
        sanitize_text(&report.config.status)
    );
    let ov = &report.config.overrides;
    let priority: Vec<String> = ov.priority_tools.iter().map(|s| sanitize_text(s)).collect();
    let disabled: Vec<String> = ov.disabled_tools.iter().map(|s| sanitize_text(s)).collect();
    let _ = writeln!(
        out,
        "  Overrides  : priority_tools=[{}], disabled_tools=[{}], history_limit={}, introspect_timeout_ms={}, sync_profile={}",
        priority.join(", "),
        disabled.join(", "),
        ov.effective_history_limit,
        ov.effective_introspect_timeout_ms,
        ov.sync_profile,
    );
    let ui_height = ov
        .ui
        .height
        .as_deref()
        .map(sanitize_text)
        .unwrap_or_else(|| "full".to_string());
    let mut env_flags = Vec::new();
    if ov.ui.no_color {
        env_flags.push("NO_COLOR");
    }
    if ov.ui.dumb_term {
        env_flags.push("TERM=dumb");
    }
    let env_note = if env_flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", env_flags.join(", "))
    };
    let _ = writeln!(
        out,
        "  UI         : theme={} (effective: {}{}), icons={}, borders={}, height={}, min_height={}",
        sanitize_text(&ov.ui.configured_theme),
        sanitize_text(&ov.ui.effective_theme),
        env_note,
        sanitize_text(&ov.ui.icons),
        sanitize_text(&ov.ui.borders),
        ui_height,
        ov.ui.effective_min_height,
    );
    let _ = writeln!(
        out,
        "  Keybindings: {} custom binding(s)",
        ov.keybindings_count
    );
    if !report.config.diagnostics.is_empty() {
        let _ = writeln!(
            out,
            "  Diagnostics: {} issue(s)",
            report.config.diagnostics.len()
        );
        for diag in &report.config.diagnostics {
            if diag.line > 0 {
                let _ = writeln!(
                    out,
                    "    - line {}: {}",
                    diag.line,
                    sanitize_text(&diag.message)
                );
            } else {
                let _ = writeln!(out, "    - {}", sanitize_text(&diag.message));
            }
        }
    }
    let _ = writeln!(out);

    let dir_state = if report.cache.dir_exists {
        "exists"
    } else {
        "missing"
    };
    let _ = writeln!(
        out,
        "Cache: {} [{dir_state}]",
        sanitize_text(&report.cache.dir)
    );
    let ps = &report.cache.ps_metadata;
    let ps_detail = match (ps.command_count, ps.age_seconds) {
        (Some(c), Some(age)) => format!(" ({c} command(s), age {age}s)"),
        (Some(c), None) => format!(" ({c} command(s))"),
        (None, Some(age)) => format!(" (age {age}s)"),
        (None, None) => String::new(),
    };
    let _ = writeln!(
        out,
        "  ps_metadata.json : {}{} — {}",
        sanitize_text(&ps.status),
        ps_detail,
        sanitize_text(&ps.path)
    );
    let cli = &report.cache.cli_cache;
    let cli_detail = match (cli.entry_count, cli.age_seconds) {
        (Some(c), Some(age)) => format!(" ({c} entry/entries, age {age}s)"),
        (Some(c), None) => format!(" ({c} entry/entries)"),
        (None, Some(age)) => format!(" (age {age}s)"),
        (None, None) => String::new(),
    };
    let _ = writeln!(
        out,
        "  cli_cache_{}.json: {}{} — {}",
        sanitize_text(&report.dialect.dialect),
        sanitize_text(&cli.status),
        cli_detail,
        sanitize_text(&cli.path)
    );
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "Declarative Specs: {} valid file(s) across {} search dir(s)",
        report.specs.loaded_files.len(),
        report.specs.search_dirs.len()
    );
    for (idx, dir) in report.specs.search_dirs.iter().enumerate() {
        let state = if dir.exists {
            format!("exists, {} valid", dir.valid_files.len())
        } else {
            "missing".to_string()
        };
        let _ = writeln!(
            out,
            "  [{}] {} ({state})",
            idx + 1,
            sanitize_text(&dir.path)
        );
    }
    for spec in &report.specs.loaded_files {
        let _ = writeln!(
            out,
            "  Loaded     : {} ({})",
            sanitize_text(&spec.command),
            sanitize_text(&spec.path)
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "Providers:");
    format_provider_line(&mut out, "pwsh", &report.providers.pwsh);
    format_provider_line(&mut out, "powershell.exe", &report.providers.powershell_exe);
    format_provider_line(&mut out, "carapace", &report.providers.carapace);
    format_provider_line(&mut out, "man", &report.providers.man);
    let _ = writeln!(out);

    let _ = writeln!(out, "Runtime Context:");
    let cwd_display = report
        .runtime
        .cwd
        .as_deref()
        .map(sanitize_text)
        .unwrap_or_else(|| "<unavailable>".to_string());
    let _ = writeln!(out, "  CWD           : {cwd_display}");
    let branch_display = report
        .runtime
        .git_branch
        .as_deref()
        .map(sanitize_text)
        .unwrap_or_else(|| "<none>".to_string());
    let _ = writeln!(out, "  Git Branch    : {branch_display}");
    match report.runtime.history.source_path.as_deref() {
        Some(src) => {
            let _ = writeln!(
                out,
                "  History       : {} entry/entries ({})",
                report.runtime.history.entry_count,
                sanitize_text(src)
            );
        }
        None => {
            let _ = writeln!(out, "  History       : none detected");
        }
    }
    if report.runtime.project_task_files.is_empty() {
        let _ = writeln!(out, "  Project Tasks : none detected");
    } else {
        let summary: Vec<String> = report
            .runtime
            .project_task_files
            .iter()
            .map(|t| format!("{} ({})", sanitize_text(&t.file), t.task_count))
            .collect();
        let _ = writeln!(out, "  Project Tasks : {}", summary.join(", "));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        root: PathBuf,
    }

    impl TempDirGuard {
        fn new(tag: &str) -> Self {
            let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = env::temp_dir().join(format!(
                "zpane_doctor_{tag}_{}_{}_{}",
                std::process::id(),
                nanos,
                seq
            ));
            fs::create_dir_all(&root).expect("create temp test dir");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn base_unix_env(root: &Path) -> DoctorEnv {
        DoctorEnv {
            is_windows: false,
            is_macos: false,
            zpane_shell: None,
            ps_module_path_present: false,
            fish_version_present: false,
            nu_version_present: false,
            shell: None,
            zpane_config: None,
            histfile: None,
            no_color: None,
            term: None,
            home: Some(root.join("home")),
            userprofile: None,
            appdata: None,
            local_appdata: None,
            xdg_config_home: Some(root.join("xdg_config")),
            xdg_cache_home: Some(root.join("xdg_cache")),
            xdg_data_home: Some(root.join("xdg_data")),
            cwd: Some(root.join("workspace")),
            now: SystemTime::now(),
        }
    }

    #[test]
    fn doctor_report_is_strictly_read_only_when_paths_are_missing() {
        let tmp = TempDirGuard::new("readonly");
        let env = base_unix_env(tmp.path());

        let report = build_doctor_report(&env, |_| None);

        assert_eq!(report.dialect.dialect, "posix");
        assert_eq!(report.dialect.label, "POSIX");
        assert_eq!(report.dialect.reason, "OS default (Unix)");
        assert!(!report.config.exists);
        assert_eq!(report.config.status, "missing");
        assert!(!report.cache.dir_exists);
        assert_eq!(report.cache.ps_metadata.status, "missing");
        assert_eq!(report.cache.cli_cache.status, "missing");
        assert!(report.specs.loaded_files.is_empty());
        assert!(!report.providers.pwsh.available);
        assert!(!report.providers.powershell_exe.available);
        assert!(!report.providers.carapace.available);
        assert!(!report.providers.man.available);
        assert!(report.runtime.git_branch.is_none());
        assert!(report.runtime.history.source_path.is_none());
        assert_eq!(report.runtime.history.entry_count, 0);
        assert!(report.runtime.project_task_files.is_empty());

        // Verify zero directories or files were created.
        assert!(!tmp.path().join("xdg_cache").exists());
        assert!(!tmp.path().join("xdg_config").exists());
        assert!(!tmp.path().join("home").exists());
        assert!(!tmp.path().join("workspace").exists());
    }

    #[test]
    fn doctor_reports_dialect_detection_reasons_across_all_sources() {
        let tmp = TempDirGuard::new("dialects");

        let mut env = base_unix_env(tmp.path());
        env.zpane_shell = Some("fish".to_string());
        let r = build_doctor_report(&env, |_| None);
        assert_eq!(r.dialect.dialect, "fish");
        assert_eq!(r.dialect.reason, "ZPANE_SHELL=fish");

        let mut env = base_unix_env(tmp.path());
        env.nu_version_present = true;
        let r = build_doctor_report(&env, |_| None);
        assert_eq!(r.dialect.dialect, "nu");
        assert_eq!(r.dialect.reason, "NU_VERSION");

        let mut env = base_unix_env(tmp.path());
        env.ps_module_path_present = true;
        let r = build_doctor_report(&env, |_| None);
        assert_eq!(r.dialect.dialect, "pwsh");
        assert_eq!(r.dialect.reason, "PSModulePath");

        let mut env = base_unix_env(tmp.path());
        env.is_windows = true;
        let r = build_doctor_report(&env, |_| None);
        assert_eq!(r.dialect.dialect, "pwsh");
        assert_eq!(r.dialect.reason, "OS default (Windows)");

        let mut env = base_unix_env(tmp.path());
        env.fish_version_present = true;
        let r = build_doctor_report(&env, |_| None);
        assert_eq!(r.dialect.dialect, "fish");
        assert_eq!(r.dialect.reason, "FISH_VERSION");

        let mut env = base_unix_env(tmp.path());
        env.shell = Some("/usr/bin/nu".to_string());
        let r = build_doctor_report(&env, |_| None);
        assert_eq!(r.dialect.dialect, "nu");
        assert_eq!(r.dialect.reason, "SHELL=/usr/bin/nu");
    }

    #[test]
    fn doctor_collects_full_config_cache_specs_providers_and_runtime_context() {
        let tmp = TempDirGuard::new("full");
        let root = tmp.path();

        // 1. Config with overrides + one non-fatal warning + NO_COLOR override
        let cfg_path = root.join("custom_config.toml");
        fs::write(
            &cfg_path,
            r#"
[discovery]
priority_tools = ["git", "cargo"]
disabled_tools = ["blocked-cli"]
history_limit = 20
introspect_timeout_ms = 1200
sync_profile = true
unknown_key = 42

[ui]
theme = "light"
icons = "ascii"
borders = "plain"
height = "45%"
min_height = 16

[keybindings]
confirm = "ctrl+y"
toggle_inspector = "f2"
"#,
        )
        .unwrap();

        // 2. Cache files (ps_metadata.json + cli_cache_posix.json)
        let cache_dir = root.join("xdg_cache").join("zpane");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(
            cache_dir.join("ps_metadata.json"),
            serde_json::json!([
                {"name": "Get-Process", "kind": "Cmdlet", "desc": "Gets processes", "params": []},
                {"name": "Get-ChildItem", "kind": "Cmdlet", "desc": "Gets items", "params": []}
            ])
            .to_string(),
        )
        .unwrap();
        fs::write(
            cache_dir.join("cli_cache_posix.json"),
            serde_json::json!({
                "git": {
                    "schema_version": 3,
                    "dialect": "posix",
                    "entries": []
                },
                "stale": {
                    "schema_version": 1,
                    "dialect": "posix",
                    "entries": []
                }
            })
            .to_string(),
        )
        .unwrap();

        // 3. Declarative specs in XDG precedence order (valid + invalid spec)
        let zpane_specs = root.join("xdg_config").join("zpane").join("specs");
        let carapace_specs = root.join("xdg_config").join("carapace").join("specs");
        fs::create_dir_all(&zpane_specs).unwrap();
        fs::create_dir_all(&carapace_specs).unwrap();
        fs::write(
            zpane_specs.join("mytool.json"),
            r#"{"name":"mytool","description":"Custom tool","flags":{"--verbose":"Verbose"}}"#,
        )
        .unwrap();
        fs::write(zpane_specs.join("broken.json"), "{not-json").unwrap();
        fs::write(
            carapace_specs.join("extra.yaml"),
            "name: extra\ndescription: Extra tool\nflags:\n  --check: Check mode\n",
        )
        .unwrap();

        // 4. Runtime context: git branch, shell history, project task files
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join(".git").join("refs").join("heads")).unwrap();
        fs::write(
            workspace.join(".git").join("HEAD"),
            "ref: refs/heads/feat/doctor\n",
        )
        .unwrap();
        fs::write(
            workspace.join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest"}}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("justfile"),
            "# Run checks\ncheck:\n    cargo test\n",
        )
        .unwrap();
        fs::write(workspace.join("Makefile"), "all:\n\techo ok\n").unwrap();
        fs::write(
            workspace.join("mise.toml"),
            "[tasks]\nlint = \"cargo clippy\"\n",
        )
        .unwrap();

        let hist_file = root.join("custom_hist");
        fs::write(
            &hist_file,
            ": 1700000001:0;git status\n: 1700000002:0;cargo test\n",
        )
        .unwrap();

        let mut env = base_unix_env(root);
        env.zpane_config = Some(cfg_path.to_string_lossy().into_owned());
        env.no_color = Some("1".to_string());
        env.histfile = Some(hist_file.to_string_lossy().into_owned());
        env.now = SystemTime::now() + Duration::from_secs(5);

        let report = build_doctor_report(&env, |exe| match exe {
            "carapace" => Some(PathBuf::from("/usr/local/bin/carapace")),
            "man" => Some(PathBuf::from("/usr/bin/man")),
            _ => None,
        });

        // Config assertions
        assert!(report.config.exists);
        assert_eq!(report.config.status, "warnings");
        assert_eq!(report.config.diagnostics.len(), 1);
        assert!(report.config.diagnostics[0].message.contains("unknown_key"));
        assert_eq!(report.config.overrides.priority_tools, vec!["git", "cargo"]);
        assert_eq!(report.config.overrides.disabled_tools, vec!["blocked-cli"]);
        assert_eq!(report.config.overrides.history_limit, Some(20));
        assert_eq!(report.config.overrides.effective_history_limit, 20);
        assert_eq!(report.config.overrides.introspect_timeout_ms, Some(1200));
        assert_eq!(
            report.config.overrides.effective_introspect_timeout_ms,
            1200
        );
        assert!(report.config.overrides.sync_profile);
        assert_eq!(report.config.overrides.ui.configured_theme, "light");
        assert_eq!(report.config.overrides.ui.effective_theme, "none");
        assert!(report.config.overrides.ui.no_color);
        assert!(!report.config.overrides.ui.dumb_term);
        assert_eq!(report.config.overrides.ui.icons, "ascii");
        assert_eq!(report.config.overrides.ui.borders, "plain");
        assert_eq!(report.config.overrides.ui.height.as_deref(), Some("45%"));
        assert_eq!(report.config.overrides.ui.effective_min_height, 16);
        assert_eq!(report.config.overrides.keybindings_count, 2);

        // Cache assertions
        assert!(report.cache.dir_exists);
        assert_eq!(report.cache.ps_metadata.status, "ok");
        assert_eq!(report.cache.ps_metadata.command_count, Some(2));
        assert!(report.cache.ps_metadata.age_seconds.is_some());
        assert_eq!(report.cache.cli_cache.status, "ok");
        assert_eq!(report.cache.cli_cache.entry_count, Some(1));

        // Specs assertions (precedence order: zpane config -> carapace config -> legacy cache)
        assert_eq!(report.specs.search_dirs.len(), 3);
        assert_eq!(report.specs.search_dirs[0].valid_files, vec!["mytool.json"]);
        assert_eq!(report.specs.search_dirs[1].valid_files, vec!["extra.yaml"]);
        assert!(!report.specs.search_dirs[2].exists);
        let loaded_cmds: Vec<&str> = report
            .specs
            .loaded_files
            .iter()
            .map(|s| s.command.as_str())
            .collect();
        assert_eq!(loaded_cmds, vec!["mytool", "extra"]);

        // Provider assertions
        assert!(!report.providers.pwsh.available);
        assert!(!report.providers.powershell_exe.available);
        assert!(report.providers.carapace.available);
        assert_eq!(
            report.providers.carapace.path.as_deref(),
            Some("/usr/local/bin/carapace")
        );
        assert!(report.providers.man.available);

        // Runtime assertions
        assert_eq!(report.runtime.git_branch.as_deref(), Some("feat/doctor"));
        assert_eq!(report.runtime.history.entry_count, 2);
        assert!(report
            .runtime
            .history
            .source_path
            .as_deref()
            .is_some_and(|p| p.ends_with("custom_hist")));
        let task_summary: Vec<(&str, usize)> = report
            .runtime
            .project_task_files
            .iter()
            .map(|t| (t.file.as_str(), t.task_count))
            .collect();
        assert_eq!(
            task_summary,
            vec![
                ("package.json", 2),
                ("justfile", 1),
                ("Makefile", 1),
                ("mise.toml", 1),
            ]
        );

        // Human & JSON formatting sanity
        let human = format_doctor_human(&report);
        assert!(human.contains("zpane doctor"));
        assert!(human.contains("Shell Dialect: POSIX (posix)"));
        assert!(human.contains("NO_COLOR"));
        assert!(human.contains("ps_metadata.json : ok (2 command(s)"));
        assert!(human.contains("cli_cache_posix.json: ok (1 entry/entries"));
        assert!(human.contains("Git Branch    : feat/doctor"));
        assert!(human.contains("package.json (2), justfile (1), Makefile (1), mise.toml (1)"));

        let json_str = serde_json::to_string_pretty(&report).expect("serialize doctor report");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("valid json");
        assert_eq!(parsed["dialect"]["dialect"], "posix");
        assert_eq!(parsed["config"]["status"], "warnings");
        assert_eq!(parsed["cache"]["cli_cache"]["entry_count"], 1);
    }

    #[test]
    fn doctor_sanitizes_hostile_escapes_and_bidi_in_json_and_human_output() {
        let tmp = TempDirGuard::new("sanitize");
        let root = tmp.path();
        let cfg_path = root.join("hostile.toml");
        fs::write(&cfg_path, "unknown_\x1b[31m\u{202e}key = 1\n").unwrap();

        let mut env = base_unix_env(root);
        env.zpane_shell = Some("posix\x1b[31m\u{202e}".to_string());
        env.zpane_config = Some(cfg_path.to_string_lossy().into_owned());

        let report = build_doctor_report(&env, |exe| {
            (exe == "man").then(|| PathBuf::from("/usr/bin/\x1b]0;pwn\x07man"))
        });

        let human = format_doctor_human(&report);
        let json = serde_json::to_string(&report).expect("serialize sanitized report");
        for output in [&human, &json] {
            assert!(!output.contains('\x1b'), "escape leaked: {output}");
            assert!(!output.contains('\x07'), "BEL leaked: {output}");
            assert!(!output.contains('\u{202e}'), "BiDi leaked: {output}");
        }
    }

    #[test]
    fn doctor_reports_invalid_caches_term_dumb_and_config_read_errors() {
        let tmp = TempDirGuard::new("invalid_states");
        let root = tmp.path();

        let bad_cfg = root.join("bad_utf8.toml");
        fs::write(&bad_cfg, [0xffu8, 0xfe, 0x00]).unwrap();

        let cache_dir = root.join("xdg_cache").join("zpane");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("ps_metadata.json"), b"{not an array").unwrap();
        fs::write(cache_dir.join("cli_cache_posix.json"), b"not json").unwrap();

        let mut env = base_unix_env(root);
        env.zpane_config = Some(bad_cfg.to_string_lossy().into_owned());
        env.term = Some("dumb".to_string());

        let report = build_doctor_report(&env, |_| None);
        assert!(report.config.exists);
        assert_eq!(report.config.status, "error");
        assert!(!report.config.diagnostics.is_empty());
        assert!(report.config.overrides.ui.dumb_term);
        assert_eq!(report.config.overrides.ui.effective_theme, "none");
        assert_eq!(report.cache.ps_metadata.status, "invalid");
        assert_eq!(report.cache.ps_metadata.command_count, None);
        assert_eq!(report.cache.cli_cache.status, "invalid");
        assert_eq!(report.cache.cli_cache.entry_count, None);
    }
}
