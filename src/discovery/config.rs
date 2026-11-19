use super::catalog::{cache_dir, non_empty_abs_env_path, read_bounded_string};
use super::history::MAX_HISTORY_ITEMS;
use crate::sanitize::sanitize_single_line;
use std::env;
use std::path::PathBuf;

pub(super) const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CONFIG_ARRAY_ITEMS: usize = 1_024;

/// Optional user overrides loaded from `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserConfig {
    pub priority_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub history_limit: Option<usize>,
}

/// Location of `zpane`'s user configuration file (`ZPANE_CONFIG`, `%LOCALAPPDATA%\zpane\config.toml`,
/// `$XDG_CONFIG_HOME/zpane/config.toml`, or `~/.config/zpane/config.toml`).
pub fn config_path() -> PathBuf {
    if let Ok(explicit) = env::var("ZPANE_CONFIG") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() && !trimmed.contains('\0') {
            return PathBuf::from(trimmed);
        }
    }
    if cfg!(windows) {
        non_empty_abs_env_path("LOCALAPPDATA")
            .or_else(|| {
                non_empty_abs_env_path("USERPROFILE").map(|p| p.join("AppData").join("Local"))
            })
            .unwrap_or_else(env::temp_dir)
            .join("zpane")
            .join("config.toml")
    } else if let Some(xdg) = non_empty_abs_env_path("XDG_CONFIG_HOME") {
        xdg.join("zpane").join("config.toml")
    } else if let Some(home) = non_empty_abs_env_path("HOME") {
        home.join(".config").join("zpane").join("config.toml")
    } else {
        cache_dir().join("config.toml")
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_toml_string_array(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let mut out = Vec::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut val = String::new();
            let mut escaped = false;
            for c in chars.by_ref() {
                if escaped {
                    val.push(c);
                    escaped = false;
                } else if c == '\\' && quote == '"' {
                    escaped = true;
                } else if c == quote {
                    break;
                } else {
                    val.push(c);
                }
            }
            let sanitized = sanitize_single_line(val.trim());
            if !sanitized.is_empty() {
                out.push(sanitized.into_owned());
                if out.len() >= MAX_CONFIG_ARRAY_ITEMS {
                    break;
                }
            }
        }
    }
    out
}

/// Parses `config.toml` supporting `priority_tools`, `disabled_tools`, and `history_limit`.
pub fn parse_user_config_toml(raw: &str) -> UserConfig {
    let mut cfg = UserConfig::default();
    if raw.len() as u64 > MAX_CONFIG_BYTES {
        return cfg;
    }
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw
        .lines()
        .map(strip_toml_comment)
        .map(str::trim)
        .filter(|l| !l.is_empty());

    let mut array_buf = String::new();
    while let Some(line) = lines.next() {
        if line.starts_with('[') {
            continue;
        }
        let Some((key_raw, val_raw)) = line.split_once('=') else {
            continue;
        };
        let key = key_raw.trim();
        let val_start = val_raw.trim();
        let is_known = matches!(key, "priority_tools" | "disabled_tools" | "history_limit");
        if val_start.starts_with('[') && !val_start.ends_with(']') {
            if is_known {
                array_buf.clear();
                array_buf.push_str(val_start);
            }
            for next_line in lines.by_ref() {
                if is_known {
                    array_buf.push(' ');
                    array_buf.push_str(next_line);
                }
                if next_line.ends_with(']') {
                    break;
                }
            }
            if !is_known {
                continue;
            }
        } else if !is_known {
            continue;
        } else {
            array_buf.clear();
            array_buf.push_str(val_start);
        }

        match key {
            "priority_tools" => cfg.priority_tools = parse_toml_string_array(&array_buf),
            "disabled_tools" => cfg.disabled_tools = parse_toml_string_array(&array_buf),
            "history_limit" => {
                let raw_num = array_buf.trim();
                let limit_opt = if !raw_num.starts_with('_')
                    && !raw_num.ends_with('_')
                    && !raw_num.contains("__")
                {
                    if raw_num.contains('_') {
                        raw_num.replace('_', "").parse::<usize>().ok()
                    } else {
                        raw_num.parse::<usize>().ok()
                    }
                } else {
                    raw_num.parse::<usize>().ok()
                };
                if let Some(limit) = limit_opt {
                    cfg.history_limit = Some(limit.min(MAX_HISTORY_ITEMS));
                }
            }
            _ => {}
        }
    }

    cfg
}

/// Loads and parses the user configuration file from [`config_path`], returning defaults on
/// missing, oversized, or unreadable files.
pub fn load_user_config() -> UserConfig {
    read_bounded_string(&config_path(), MAX_CONFIG_BYTES)
        .map(|raw| parse_user_config_toml(&raw))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_config_toml_supports_comments_multiline_arrays_and_underscores() {
        let raw = r#"
# Custom zpane preferences
history_limit = 25
priority_tools = [
  "mycli",
  "gs",
]
disabled_tools = ["blocked"] # hide unwanted binaries
"#;
        assert_eq!(
            parse_user_config_toml(raw),
            UserConfig {
                priority_tools: vec!["mycli".to_string(), "gs".to_string()],
                disabled_tools: vec!["blocked".to_string()],
                history_limit: Some(25),
            }
        );

        let with_bom = "\u{feff}history_limit = 1_500\npriority_tools = [\" mycli \"]\n";
        assert_eq!(
            parse_user_config_toml(with_bom),
            UserConfig {
                priority_tools: vec!["mycli".to_string()],
                disabled_tools: Vec::new(),
                history_limit: Some(1500),
            }
        );

        let clamped = parse_user_config_toml("history_limit = 999999\n");
        assert_eq!(clamped.history_limit, Some(MAX_HISTORY_ITEMS));
    }
}
