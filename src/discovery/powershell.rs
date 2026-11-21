use super::catalog::{
    cache_dir, ensure_secure_dir, hide_window, home_dir, is_safe_command_token,
    non_empty_abs_env_path, read_bounded_bytes, read_bounded_string_lossy, write_secure_file,
    ShellDialect,
};
use super::config::load_user_config;
use super::introspect::find_executable_in_path;
use crate::model::{Entry, EntryKind};
use crate::sanitize::{sanitize_multiline_desc, sanitize_single_line, strip_help_text_escapes};
use crate::shell::quote_literal_for_dialect;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub(super) const MAX_PS_METADATA_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MAML_HELP_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MAML_SCAN_FILES: usize = 256;
const MAX_MAML_SCAN_DEPTH: usize = 6;
const MAX_MAML_DESC_CHARS: usize = 4_096;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct PsParamExport {
    pub(super) name: String,
    #[serde(default)]
    pub(super) aliases: Vec<String>,
    #[serde(default)]
    pub(super) type_name: String,
    #[serde(default)]
    pub(super) is_switch: bool,
    #[serde(default)]
    pub(super) is_common: bool,
    #[serde(default)]
    pub(super) valid_values: Vec<String>,
    #[serde(default)]
    pub(super) argument_completions: Vec<String>,
    #[serde(default)]
    pub(super) parameter_sets: Vec<String>,
    #[serde(default)]
    pub(super) mandatory_sets: Vec<String>,
    #[serde(default)]
    pub(super) position: Option<i32>,
    #[serde(default)]
    pub(super) accepts_pipeline: bool,
    #[serde(default)]
    pub(super) accepts_pipeline_by_property_name: bool,
    #[serde(default)]
    pub(super) validate_range: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct PsCommandExport {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) desc: String,
    #[serde(default)]
    pub(super) help_file: Option<String>,
    #[serde(default)]
    pub(super) params: Vec<PsParamExport>,
}

/// Extracted documentation for a single PowerShell command and its parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PsCmdHelp {
    pub synopsis: String,
    pub description: String,
    pub params: HashMap<String, String>,
}

pub(super) fn ps_metadata_path() -> PathBuf {
    cache_dir().join("ps_metadata.json")
}

fn format_param_desc(
    p_type: &str,
    label: &str,
    p: &PsParamExport,
    clean_range: Option<&str>,
) -> String {
    let mut desc = format!("[{p_type}] Parameter {label}");
    let mut hints = Vec::new();
    if !p.mandatory_sets.is_empty() {
        hints.push("Required".to_string());
    }
    if let Some(pos) = p.position.filter(|&pos| pos >= 0) {
        hints.push(format!("Pos: {pos}"));
    }
    match (p.accepts_pipeline, p.accepts_pipeline_by_property_name) {
        (true, true) => hints.push("Pipe: ByValue, ByName".to_string()),
        (true, false) => hints.push("Pipe: ByValue".to_string()),
        (false, true) => hints.push("Pipe: ByName".to_string()),
        (false, false) => {}
    }
    if let Some(range) = clean_range {
        hints.push(format!("Range: {range}"));
    }
    if !hints.is_empty() {
        let _ = write!(desc, " ({})", hints.join(", "));
    }
    desc
}

fn convert_ps_export_to_entry(exp: PsCommandExport) -> Entry {
    let kind = match exp.kind.as_str() {
        "Func" => EntryKind::Func,
        "Alias" => EntryKind::Alias,
        "CLI" => EntryKind::Cli,
        _ => EntryKind::Cmdlet,
    };

    let cmd_name = exp.name.trim();
    let cmd_desc = sanitize_single_line(&exp.desc);
    let help_file = exp
        .help_file
        .as_deref()
        .map(|s| sanitize_single_line(s.trim()).into_owned())
        .filter(|s| is_safe_help_filename(s));
    let mut cmd_entry =
        Entry::new(cmd_name, cmd_name, kind, cmd_desc.as_ref()).with_help_file(help_file);
    let mut primary = Vec::with_capacity(exp.params.len());
    let mut common = Vec::new();

    for p in exp.params {
        let p_name = sanitize_single_line(p.name.trim());
        if p_name.is_empty() || p_name.chars().any(char::is_whitespace) {
            continue;
        }
        let p_flag = format!("-{p_name}");
        let clean_aliases: Vec<String> = p
            .aliases
            .iter()
            .filter_map(|a| {
                let s = sanitize_single_line(a.trim());
                (!s.is_empty() && !s.chars().any(char::is_whitespace)).then(|| s.into_owned())
            })
            .collect();
        let label = if clean_aliases.is_empty() {
            p_flag.clone()
        } else {
            let mut buf = String::with_capacity(p_flag.len() + clean_aliases.len() * 6 + 4);
            buf.push_str(&p_flag);
            buf.push_str(" (");
            for (idx, a) in clean_aliases.iter().enumerate() {
                if idx > 0 {
                    buf.push_str(", ");
                }
                buf.push('-');
                buf.push_str(a);
            }
            buf.push(')');
            buf
        };
        let p_kind = if p.is_switch {
            EntryKind::Flag
        } else {
            EntryKind::Param
        };
        let p_type = sanitize_single_line(&p.type_name);
        let clean_range = p
            .validate_range
            .as_deref()
            .map(|r| sanitize_single_line(r.trim()).into_owned())
            .filter(|r| !r.is_empty());
        let desc = format_param_desc(p_type.as_ref(), &label, &p, clean_range.as_deref());

        let clean_param_sets: Vec<String> = p
            .parameter_sets
            .into_iter()
            .filter_map(|s| {
                let clean = sanitize_single_line(s.trim()).into_owned();
                (!clean.is_empty()).then_some(clean)
            })
            .collect();
        let clean_mandatory_sets: Vec<String> = p
            .mandatory_sets
            .into_iter()
            .filter_map(|s| {
                let clean = sanitize_single_line(s.trim()).into_owned();
                (!clean.is_empty()).then_some(clean)
            })
            .collect();

        let mut param_entry = Entry::new(&p_flag, label, p_kind, desc)
            .with_parameter_sets(clean_param_sets)
            .with_mandatory_sets(clean_mandatory_sets)
            .with_position(p.position.filter(|&pos| pos >= 0))
            .with_pipeline(p.accepts_pipeline, p.accepts_pipeline_by_property_name)
            .with_validate_range(clean_range)
            .with_aliases(clean_aliases);

        param_entry.children.reserve(
            p.valid_values.len() + p.argument_completions.len() + if p.is_switch { 3 } else { 0 },
        );
        let mut seen_vals =
            HashSet::with_capacity(p.valid_values.len() + p.argument_completions.len());
        for val in p.valid_values {
            let clean_val = sanitize_single_line(val.trim());
            let trimmed = clean_val.trim();
            if !trimmed.is_empty() && seen_vals.insert(trimmed.to_ascii_lowercase()) {
                let token =
                    quote_literal_for_dialect(trimmed, Some(ShellDialect::PowerShell), true);
                param_entry.children.push(Entry::new(
                    token,
                    trimmed,
                    EntryKind::Enum,
                    format!("Allowed [{p_type}] value for {p_flag}"),
                ));
            }
        }
        for comp in p.argument_completions {
            let clean_comp = sanitize_single_line(comp.trim());
            let trimmed = clean_comp.trim();
            if !trimmed.is_empty() && seen_vals.insert(trimmed.to_ascii_lowercase()) {
                let token =
                    quote_literal_for_dialect(trimmed, Some(ShellDialect::PowerShell), true);
                param_entry.children.push(Entry::new(
                    token,
                    trimmed,
                    EntryKind::Value,
                    format!("Suggested [{p_type}] completion for {p_flag}"),
                ));
            }
        }
        if p.is_switch {
            param_entry.children.push(Entry::placeholder(
                "(Flag - No Value Needed)",
                EntryKind::Flag,
                format!("Press Enter or Tab to include {p_flag}"),
            ));
            param_entry.children.push(Entry::new(
                ":$true",
                ":$true",
                EntryKind::Value,
                "Explicitly enable switch",
            ));
            param_entry.children.push(Entry::new(
                ":$false",
                ":$false",
                EntryKind::Value,
                "Explicitly disable switch",
            ));
        }

        if p.is_common {
            common.push(param_entry);
        } else {
            primary.push(param_entry);
        }
    }

    primary.sort_by_key(|p| p.mandatory_sets.is_empty());
    primary.extend(common);
    cmd_entry.children = primary;
    cmd_entry
}

/// Parses serialized `ps_metadata.json` bytes into validated catalog [`Entry`] values.
pub(super) fn parse_ps_metadata_entries(bytes: &[u8]) -> Vec<Entry> {
    let Ok(exports) = serde_json::from_slice::<Vec<PsCommandExport>>(bytes) else {
        return Vec::new();
    };
    exports
        .into_iter()
        .filter(|exp| is_safe_command_token(exp.name.trim()))
        .map(convert_ps_export_to_entry)
        .collect()
}

/// Counts valid PowerShell command records in serialized `ps_metadata.json` bytes.
/// Returns `Err(())` when `bytes` is not a valid metadata JSON array.
pub(super) fn count_ps_metadata_commands(bytes: &[u8]) -> Result<usize, ()> {
    let exports = serde_json::from_slice::<Vec<PsCommandExport>>(bytes).map_err(|_| ())?;
    Ok(exports
        .iter()
        .filter(|exp| is_safe_command_token(exp.name.trim()))
        .count())
}

/// Resolves the PowerShell executable to run: prefers `pwsh`, and on Windows falls back to
/// `powershell.exe` when `pwsh` is absent from `PATH`.
pub(super) fn resolve_powershell_executable_with<F>(
    is_windows: bool,
    mut find_exe: F,
) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<PathBuf>,
{
    if let Some(pwsh) = find_exe("pwsh") {
        return Some(pwsh);
    }
    if is_windows {
        return find_exe("powershell.exe").or_else(|| find_exe("powershell"));
    }
    None
}

pub(super) fn resolve_powershell_executable() -> Option<PathBuf> {
    resolve_powershell_executable_with(cfg!(windows), find_executable_in_path)
}

const PS_METADATA_SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$commonSet = [System.Collections.Generic.HashSet[string]]::new(
    [string[]]@('Verbose','Debug','ErrorAction','WarningAction','InformationAction','ProgressAction','ErrorVariable','WarningVariable','InformationVariable','OutVariable','OutBuffer','PipelineVariable','WhatIf','Confirm'),
    [System.StringComparer]::OrdinalIgnoreCase
)
$argCompType = 'System.Management.Automation.ArgumentCompletionsAttribute' -as [type]
$cmds = Get-Command -CommandType Function, Alias, Cmdlet
$out = [System.Collections.Generic.List[object]]::new($cmds.Count)
foreach ($c in $cmds) {
    if ($c.Name -match '^(__|TabExpansion|Prompt|Clear-Host|more|oss|help)$') { continue }
    $kind = switch ($c.CommandType) {
        'Function' { 'Func' }
        'Alias'    { 'Alias' }
        default    { 'Cmdlet' }
    }
    $target = if ($c.CommandType -eq 'Alias' -and $c.ResolvedCommand) { $c.ResolvedCommand } else { $c }
    $desc = if ($c.CommandType -eq 'Alias') { "Alias -> $($target.Name)" } elseif ($c.Source) { "Module: $($c.Source)" } else { "PowerShell $kind" }
    $helpFile = if ($target.HelpFile) { [string]$target.HelpFile } else { $null }
    $pList = [System.Collections.Generic.List[object]]::new()
    if ($target.Parameters) {
        foreach ($p in $target.Parameters.Values) {
            $vals = [System.Collections.Generic.List[string]]::new()
            $comps = [System.Collections.Generic.List[string]]::new()
            $rangeStr = $null
            if ($p.ParameterType.IsEnum) {
                foreach ($n in [System.Enum]::GetNames($p.ParameterType)) { $vals.Add($n) }
            }
            foreach ($attr in $p.Attributes) {
                if ($attr -is [System.Management.Automation.ValidateSetAttribute]) {
                    foreach ($v in $attr.ValidValues) { if (-not $vals.Contains($v)) { $vals.Add($v) } }
                } elseif ($argCompType -and ($attr -is $argCompType) -and $attr.Completions) {
                    foreach ($v in $attr.Completions) {
                        if ($null -ne $v) {
                            $s = [string]$v
                            if (-not [string]::IsNullOrWhiteSpace($s) -and -not $comps.Contains($s)) { $comps.Add($s) }
                        }
                    }
                } elseif ($attr -is [System.Management.Automation.ValidateRangeAttribute]) {
                    if ($null -ne $attr.MinRange -or $null -ne $attr.MaxRange) {
                        $rangeStr = "$($attr.MinRange)..$($attr.MaxRange)"
                    }
                }
            }
            $mandatorySets = [System.Collections.Generic.List[string]]::new()
            $pos = $null
            $pipeVal = $false
            $pipeName = $false
            foreach ($kv in $p.ParameterSets.GetEnumerator()) {
                $meta = $kv.Value
                if ($meta.IsMandatory) { $mandatorySets.Add($kv.Key) }
                if ($meta.Position -ge 0 -and ($null -eq $pos -or $meta.Position -lt $pos)) {
                    $pos = [int]$meta.Position
                }
                if ($meta.ValueFromPipeline) { $pipeVal = $true }
                if ($meta.ValueFromPipelineByPropertyName) { $pipeName = $true }
            }
            $paramSets = [string[]]@($p.ParameterSets.Keys)
            $pEntry = @{
                name                              = $p.Name
                aliases                           = @($p.Aliases)
                type_name                         = ($p.ParameterType.Name -replace '`1$', '')
                is_switch                         = [bool]$p.SwitchParameter
                is_common                         = [bool]$commonSet.Contains($p.Name)
                valid_values                      = $vals
                argument_completions              = $comps
                parameter_sets                    = $paramSets
                mandatory_sets                    = $mandatorySets
                accepts_pipeline                  = $pipeVal
                accepts_pipeline_by_property_name = $pipeName
            }
            if ($null -ne $pos) { $pEntry['position'] = $pos }
            if ($null -ne $rangeStr) { $pEntry['validate_range'] = $rangeStr }
            $pList.Add($pEntry)
        }
    }
    $cmdObj = @{
        name   = $c.Name
        kind   = $kind
        desc   = $desc
        params = $pList
    }
    if ($helpFile) { $cmdObj['help_file'] = $helpFile }
    $out.Add($cmdObj)
}
$json = if ($out.Count -eq 0) { '[]' } else { ConvertTo-Json -InputObject @($out) -Depth 6 -Compress }
if ($env:ZPANE_PS_META_OUT) {
    [System.IO.File]::WriteAllText($env:ZPANE_PS_META_OUT, $json, [System.Text.UTF8Encoding]::new($false))
} else {
    $json
}
"#;

struct TempFileGuard {
    path: PathBuf,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_secure_temp_file(dir: &Path) -> std::io::Result<TempFileGuard> {
    ensure_secure_dir(dir)?;
    static PS_SYNC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = PS_SYNC_TMP_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    for attempt in 0..8u64 {
        let candidate = dir.join(format!(".ps_metadata_sync.{pid}.{seq}.{attempt}.tmp"));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        match opts.open(&candidate) {
            Ok(_) => return Ok(TempFileGuard { path: candidate }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate unique temp file for PowerShell metadata sync",
    ))
}

fn sync_powershell_metadata_with_boundary<F, R>(
    sync_profile: bool,
    is_windows: bool,
    dest_path: &Path,
    temp_dir: &Path,
    find_exe: F,
    run_ps: R,
) -> anyhow::Result<usize>
where
    F: FnMut(&str) -> Option<PathBuf>,
    R: FnOnce(&Path, &[&str], Option<&Path>) -> anyhow::Result<Vec<u8>>,
{
    let exe_path = resolve_powershell_executable_with(is_windows, find_exe).ok_or_else(|| {
        let msg = if is_windows {
            "pwsh or powershell.exe executable not found on PATH"
        } else {
            "pwsh executable not found on PATH"
        };
        std::io::Error::new(std::io::ErrorKind::NotFound, msg)
    })?;

    let (raw_json, _guard) = if sync_profile {
        let guard = create_secure_temp_file(temp_dir)?;
        let args = ["-NoLogo", "-Command", PS_METADATA_SCRIPT];
        let _stdout_ignored = run_ps(&exe_path, &args, Some(&guard.path))?;
        let bytes = read_bounded_bytes(&guard.path, MAX_PS_METADATA_BYTES).ok_or_else(|| {
            std::io::Error::other("PowerShell profile sync did not produce bounded metadata JSON")
        })?;
        (bytes, Some(guard))
    } else {
        let args = ["-NoLogo", "-NoProfile", "-Command", PS_METADATA_SCRIPT];
        let stdout_buf = run_ps(&exe_path, &args, None)?;
        (stdout_buf, None)
    };

    if (raw_json.len() as u64) > MAX_PS_METADATA_BYTES {
        anyhow::bail!("pwsh metadata export exceeded {MAX_PS_METADATA_BYTES} byte limit");
    }

    let exports: Vec<PsCommandExport> = serde_json::from_slice(&raw_json)?;
    let count = exports.len();
    write_secure_file(dest_path, &raw_json)?;
    Ok(count)
}

/// Synchronizes PowerShell Cmdlets, Functions, Aliases, and Parameter Metadata to disk
/// using an explicit `sync_profile` flag (bypassing `-NoProfile` via a secure temp file when `true`).
pub fn sync_powershell_metadata_with_config(sync_profile: bool) -> anyhow::Result<usize> {
    let dest = ps_metadata_path();
    let dir = cache_dir();
    sync_powershell_metadata_with_boundary(
        sync_profile,
        cfg!(windows),
        &dest,
        &dir,
        find_executable_in_path,
        |exe_path, args, out_file| {
            let mut cmd = Command::new(exe_path);
            cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
            if let Some(out_path) = out_file {
                cmd.env("ZPANE_PS_META_OUT", out_path).stdout(Stdio::null());
            } else {
                cmd.env_remove("ZPANE_PS_META_OUT").stdout(Stdio::piped());
            }
            let mut child = hide_window(&mut cmd).spawn()?;
            let mut stdout_buf = Vec::new();
            if out_file.is_none() {
                if let Some(stdout) = child.stdout.take() {
                    stdout
                        .take(MAX_PS_METADATA_BYTES.saturating_add(1))
                        .read_to_end(&mut stdout_buf)?;
                }
            }
            let status = child.wait()?;
            if (stdout_buf.len() as u64) > MAX_PS_METADATA_BYTES {
                anyhow::bail!("pwsh metadata export exceeded {MAX_PS_METADATA_BYTES} byte limit");
            }
            if !status.success() {
                return Err(std::io::Error::other(format!(
                    "PowerShell metadata export failed with status {status}"
                ))
                .into());
            }
            Ok(stdout_buf)
        },
    )
}

/// Synchronizes PowerShell Cmdlets, Functions, Aliases, and Parameter Metadata to disk,
/// honoring `sync_profile` in `config.toml` and falling back to `powershell.exe` on Windows.
pub fn sync_powershell_metadata() -> anyhow::Result<usize> {
    sync_powershell_metadata_with_config(load_user_config().sync_profile)
}

fn is_safe_help_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
}

fn find_ascii_ignore_case(haystack: &str, needle_lower: &str, start: usize) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() {
        return Some(start);
    }
    if start >= h.len() || h.len() - start < n.len() {
        return None;
    }
    let first = n[0];
    let max = h.len() - n.len();
    for i in start..=max {
        if h[i].to_ascii_lowercase() == first && h[i + 1..i + n.len()].eq_ignore_ascii_case(&n[1..])
        {
            return Some(i);
        }
    }
    None
}

fn find_tag_open(haystack: &str, tag_lower: &str, start: usize) -> Option<(usize, usize)> {
    let pattern = format!("<{tag_lower}");
    let mut pos = start;
    while let Some(idx) = find_ascii_ignore_case(haystack, &pattern, pos) {
        let after = idx + pattern.len();
        let next_b = haystack.as_bytes().get(after).copied();
        if next_b.is_some_and(|b| b == b'>' || b.is_ascii_whitespace()) {
            let gt_rel = haystack[after..].find('>')?;
            return Some((idx, after + gt_rel + 1));
        }
        pos = after;
    }
    None
}

fn next_tag_block<'a>(
    haystack: &'a str,
    tag_lower: &str,
    start: usize,
) -> Option<(usize, &'a str, usize)> {
    let (open_start, body_start) = find_tag_open(haystack, tag_lower, start)?;
    let close_pat = format!("</{tag_lower}>");
    let close_idx = find_ascii_ignore_case(haystack, &close_pat, body_start)?;
    let body = &haystack[body_start..close_idx];
    Some((open_start, body, close_idx + close_pat.len()))
}

fn decode_xml_entities_and_strip_tags(raw: &str, out: &mut String) {
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(cdata_rest) = rest.strip_prefix("<![CDATA[") {
            if let Some(end) = cdata_rest.find("]]>") {
                out.push_str(&cdata_rest[..end]);
                rest = &cdata_rest[end + 3..];
                continue;
            }
        }
        if let Some(comment_rest) = rest.strip_prefix("<!--") {
            if let Some(end) = comment_rest.find("-->") {
                rest = &comment_rest[end + 3..];
                continue;
            }
        }
        let mut next_special = None;
        for (i, b) in rest.bytes().enumerate() {
            if b == b'<' || b == b'&' {
                next_special = Some((i, b));
                break;
            }
        }
        let Some((idx, ch)) = next_special else {
            out.push_str(rest);
            break;
        };
        if idx > 0 {
            out.push_str(&rest[..idx]);
        }
        if ch == b'<' {
            let tail = &rest[idx + 1..];
            if let Some(gt) = tail.find('>') {
                rest = &tail[gt + 1..];
            } else {
                break;
            }
        } else {
            let tail = &rest[idx + 1..];
            if let Some(semi) = tail.find(';').filter(|&s| s <= 12) {
                let entity = &tail[..semi];
                let decoded = match entity {
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                        u32::from_str_radix(&entity[2..], 16)
                            .ok()
                            .and_then(char::from_u32)
                    }
                    _ if entity.starts_with('#') => {
                        entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(c) = decoded {
                    out.push(c);
                    rest = &tail[semi + 1..];
                    continue;
                }
            }
            out.push('&');
            rest = tail;
        }
    }
}

fn collapse_ws_into(src: &str, out: &mut String) {
    let mut first = true;
    for word in src.split_whitespace() {
        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(word);
    }
}

fn truncate_chars_bounded(mut s: String, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s;
    }
    if let Some((idx, _)) = s.char_indices().nth(max_chars) {
        s.truncate(idx);
    }
    s
}

fn extract_maml_description_text(fragment: &str, multiline: bool) -> String {
    let mut paras = Vec::new();
    let mut pos = 0usize;
    let mut scratch = String::new();
    let mut collapsed = String::new();
    while let Some((_, para_body, next_pos)) = next_tag_block(fragment, "maml:para", pos) {
        scratch.clear();
        decode_xml_entities_and_strip_tags(para_body, &mut scratch);
        collapsed.clear();
        collapse_ws_into(&scratch, &mut collapsed);
        if !collapsed.is_empty() {
            paras.push(collapsed.clone());
        }
        pos = next_pos;
    }
    if paras.is_empty() {
        scratch.clear();
        decode_xml_entities_and_strip_tags(fragment, &mut scratch);
        collapsed.clear();
        collapse_ws_into(&scratch, &mut collapsed);
        if !collapsed.is_empty() {
            paras.push(collapsed);
        }
    }
    let joined = if multiline {
        paras.join("\n\n")
    } else {
        paras.join(" ")
    };
    let stripped = strip_help_text_escapes(&joined);
    let sanitized = if multiline {
        sanitize_multiline_desc(stripped.as_ref()).into_owned()
    } else {
        sanitize_single_line(stripped.as_ref()).into_owned()
    };
    truncate_chars_bounded(sanitized, MAX_MAML_DESC_CHARS)
}

fn extract_maml_name(fragment: &str, tag_lower: &str) -> Option<String> {
    let (_, body, _) = next_tag_block(fragment, tag_lower, 0)?;
    let mut raw = String::new();
    decode_xml_entities_and_strip_tags(body, &mut raw);
    let clean = sanitize_single_line(raw.trim());
    let trimmed = clean.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_maml_parameters_into(fragment: &str, params: &mut HashMap<String, String>) {
    let mut pos = 0usize;
    while let Some((_, param_body, next_pos)) = next_tag_block(fragment, "command:parameter", pos) {
        if let Some(param_name) = extract_maml_name(param_body, "maml:name") {
            if let Some((_, desc_body, _)) = next_tag_block(param_body, "maml:description", 0) {
                let text = extract_maml_description_text(desc_body, false);
                if !text.is_empty() {
                    params
                        .entry(param_name.to_ascii_lowercase())
                        .or_insert(text);
                }
            }
        }
        pos = next_pos;
    }
}

/// Parses a PowerShell MAML `*-Help.xml` document into a lowercase command-name map of [`PsCmdHelp`].
pub(crate) fn parse_maml_help_xml(xml: &str) -> HashMap<String, PsCmdHelp> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let mut out = HashMap::new();
    let mut pos = 0usize;

    while let Some((_, cmd_body, next_pos)) = next_tag_block(xml, "command:command", pos) {
        pos = next_pos;
        let Some((_, details_body, details_end)) = next_tag_block(cmd_body, "command:details", 0)
        else {
            continue;
        };
        let Some(cmd_name) = extract_maml_name(details_body, "command:name")
            .or_else(|| extract_maml_name(details_body, "maml:name"))
        else {
            continue;
        };

        let synopsis = next_tag_block(details_body, "maml:description", 0)
            .map(|(_, d, _)| extract_maml_description_text(d, false))
            .unwrap_or_default();

        // Top-level `<maml:description>` appears immediately after `<command:details>` and before
        // `<command:syntax>` / `<command:parameters>` / `<command:examples>`.
        let after_details = &cmd_body[details_end..];
        let section_end = [
            "<command:syntax",
            "<command:parameters",
            "<command:examples",
        ]
        .iter()
        .filter_map(|tag| find_ascii_ignore_case(after_details, tag, 0))
        .min()
        .unwrap_or(after_details.len());
        let description = next_tag_block(&after_details[..section_end], "maml:description", 0)
            .map(|(_, d, _)| extract_maml_description_text(d, true))
            .unwrap_or_default();

        let mut params = HashMap::new();
        if let Some((_, params_body, _)) = next_tag_block(cmd_body, "command:parameters", 0) {
            parse_maml_parameters_into(params_body, &mut params);
        }
        if let Some((_, syntax_body, _)) = next_tag_block(cmd_body, "command:syntax", 0) {
            parse_maml_parameters_into(syntax_body, &mut params);
        }

        out.insert(
            cmd_name.to_ascii_lowercase(),
            PsCmdHelp {
                synopsis,
                description,
                params,
            },
        );
    }

    out
}

/// Collects candidate PowerShell help root directories (`PSModulePath`, `$PSHOME`/binary dir,
/// and user PowerShell Help directories).
pub(crate) fn collect_ps_help_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    let mut push_dir = |candidate: PathBuf| {
        if candidate.as_os_str().is_empty() || !candidate.is_absolute() {
            return;
        }
        let Ok(canon) = fs::canonicalize(&candidate) else {
            return;
        };
        if canon.is_dir() && seen.insert(canon) {
            roots.push(candidate);
        }
    };

    if let Some(ps_home) = non_empty_abs_env_path("PSHOME") {
        push_dir(ps_home);
    }
    if let Some(ps_exe) = resolve_powershell_executable() {
        if let Some(parent) = ps_exe.parent() {
            push_dir(parent.to_path_buf());
        }
    }
    if cfg!(windows) {
        if let Some(sys_root) = non_empty_abs_env_path("SystemRoot") {
            push_dir(
                sys_root
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0"),
            );
        }
    }
    if let Some(mod_path) = env::var_os("PSModulePath") {
        for p in env::split_paths(&mod_path) {
            push_dir(p);
        }
    }
    if let Some(home) = home_dir() {
        if cfg!(windows) {
            push_dir(home.join("Documents").join("PowerShell").join("Help"));
            push_dir(
                home.join("Documents")
                    .join("WindowsPowerShell")
                    .join("Help"),
            );
        } else {
            push_dir(
                home.join(".local")
                    .join("share")
                    .join("powershell")
                    .join("Help"),
            );
        }
    }

    roots
}

fn collect_maml_files_bounded(
    dir: &Path,
    depth: usize,
    target_filename_lower: Option<&str>,
    visited_files: &mut usize,
    seen_dirs: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    if depth > MAX_MAML_SCAN_DEPTH || *visited_files >= MAX_MAML_SCAN_FILES {
        return;
    }
    let Ok(canon) = fs::canonicalize(dir) else {
        return;
    };
    if !seen_dirs.insert(canon) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        if *visited_files >= MAX_MAML_SCAN_FILES {
            break;
        }
        let path = entry.path();
        let Ok(sym_meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if sym_meta.file_type().is_symlink() {
            continue;
        }
        if sym_meta.is_dir() {
            subdirs.push(path);
        } else if sym_meta.is_file() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            let matches = match target_filename_lower {
                Some(target) => lower == target,
                None => lower.ends_with("-help.xml"),
            };
            if matches {
                *visited_files += 1;
                out.push(path);
                if target_filename_lower.is_some() {
                    return;
                }
            }
        }
    }
    for sub in subdirs {
        if *visited_files >= MAX_MAML_SCAN_FILES {
            break;
        }
        collect_maml_files_bounded(
            &sub,
            depth + 1,
            target_filename_lower,
            visited_files,
            seen_dirs,
            out,
        );
        if target_filename_lower.is_some() && !out.is_empty() {
            return;
        }
    }
}

pub(crate) fn load_ps_maml_file_map(path: &Path) -> Option<HashMap<String, PsCmdHelp>> {
    let xml = read_bounded_string_lossy(path, MAX_MAML_HELP_BYTES)?;
    Some(parse_maml_help_xml(&xml))
}

/// Lazily locates and parses MAML `*-Help.xml` documentation for `cmd_name` across `roots`.
pub(crate) fn lookup_ps_maml_help_in_dirs(
    cmd_name: &str,
    help_file: Option<&str>,
    roots: &[PathBuf],
) -> Option<(PsCmdHelp, PathBuf)> {
    let key = cmd_name.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }

    let safe_hf_lower = help_file
        .map(str::trim)
        .filter(|hf| is_safe_help_filename(hf))
        .map(str::to_ascii_lowercase);

    let mut visited_files = 0usize;
    let mut seen_dirs = HashSet::new();

    if let Some(ref hf_lower) = safe_hf_lower {
        let mut targeted = Vec::new();
        for root in roots {
            collect_maml_files_bounded(
                root,
                0,
                Some(hf_lower.as_str()),
                &mut visited_files,
                &mut seen_dirs,
                &mut targeted,
            );
            if let Some(path) = targeted.pop() {
                if let Some(mut map) = load_ps_maml_file_map(&path) {
                    if let Some(help) = map.remove(&key) {
                        return Some((help, path));
                    }
                }
            }
        }
    }

    visited_files = 0;
    seen_dirs.clear();
    let mut candidates = Vec::new();
    for root in roots {
        collect_maml_files_bounded(
            root,
            0,
            None,
            &mut visited_files,
            &mut seen_dirs,
            &mut candidates,
        );
    }
    for path in candidates {
        if let Some(mut map) = load_ps_maml_file_map(&path) {
            if let Some(help) = map.remove(&key) {
                return Some((help, path));
            }
        }
    }
    None
}

/// Applies lazily resolved [`PsCmdHelp`] documentation onto a PowerShell command [`Entry`]
/// and its parameter children while preserving CLR type prefixes and existing fallback metadata.
pub(crate) fn apply_ps_help_to_entry(entry: &mut Entry, help: &PsCmdHelp) {
    let syn = help.synopsis.trim();
    let desc = help.description.trim();
    if !syn.is_empty() && !desc.is_empty() && !syn.eq_ignore_ascii_case(desc) {
        entry.desc = format!("{syn}\n\n{desc}");
    } else if !syn.is_empty() {
        entry.desc = syn.to_string();
    } else if !desc.is_empty() {
        entry.desc = desc.to_string();
    }

    if help.params.is_empty() {
        return;
    }

    for child in &mut entry.children {
        let Some(param_name) = child.text.strip_prefix('-') else {
            continue;
        };
        let key = param_name.to_ascii_lowercase();
        let Some(doc) = help.params.get(&key) else {
            continue;
        };
        let doc_trimmed = doc.trim();
        if doc_trimmed.is_empty() {
            continue;
        }
        if child.desc.starts_with('[') {
            if let Some(end_bracket) = child.desc.find(']') {
                let type_prefix = &child.desc[..=end_bracket];
                child.desc = format!("{type_prefix} {doc_trimmed}");
                continue;
            }
        }
        child.desc = doc_trimmed.to_string();
    }
}

#[derive(Debug, Deserialize)]
struct GetHelpParamJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    desc: String,
}

#[derive(Debug, Deserialize)]
struct GetHelpJson {
    #[serde(default)]
    synopsis: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    params: Vec<GetHelpParamJson>,
}

pub(crate) fn parse_get_help_json(raw: &str) -> Option<PsCmdHelp> {
    let trimmed = raw.trim();
    let start = trimmed.find('{')?;
    let parsed: GetHelpJson = serde_json::from_str(&trimmed[start..]).ok()?;
    let synopsis = sanitize_single_line(parsed.synopsis.trim()).into_owned();
    let description = sanitize_multiline_desc(parsed.description.trim()).into_owned();
    let mut params = HashMap::new();
    for p in parsed.params {
        let k = sanitize_single_line(p.name.trim()).to_ascii_lowercase();
        let v = sanitize_single_line(p.desc.trim()).into_owned();
        if !k.is_empty() && !v.is_empty() {
            params.insert(k, v);
        }
    }
    if synopsis.is_empty() && description.is_empty() && params.is_empty() {
        return None;
    }
    Some(PsCmdHelp {
        synopsis,
        description,
        params,
    })
}

pub(crate) fn is_safe_ps_help_cmd_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryRole;
    use std::path::Path;

    #[test]
    fn test_convert_ps_export_preserves_structured_aliases_and_flag_role() {
        let export = PsCommandExport {
            name: "Get-Process".to_string(),
            kind: "Cmdlet".to_string(),
            desc: "Module: Microsoft.PowerShell.Management".to_string(),
            params: vec![
                PsParamExport {
                    name: "Id".to_string(),
                    aliases: vec!["PID".to_string(), "ProcessId".to_string()],
                    type_name: "Int32[]".to_string(),
                    is_switch: false,
                    is_common: false,
                    valid_values: Vec::new(),
                    parameter_sets: vec!["Id".to_string()],
                    ..PsParamExport::default()
                },
                PsParamExport {
                    name: "Force".to_string(),
                    aliases: vec!["f".to_string()],
                    type_name: "SwitchParameter".to_string(),
                    is_switch: true,
                    is_common: false,
                    valid_values: Vec::new(),
                    parameter_sets: vec!["__AllParameterSets".to_string()],
                    ..PsParamExport::default()
                },
                PsParamExport {
                    name: "InformationAction".to_string(),
                    aliases: vec!["ia".to_string()],
                    type_name: "ActionPreference".to_string(),
                    is_switch: false,
                    is_common: true,
                    valid_values: vec!["SilentlyContinue".to_string(), "Continue".to_string()],
                    parameter_sets: vec!["__AllParameterSets".to_string()],
                    ..PsParamExport::default()
                },
            ],
            ..PsCommandExport::default()
        };

        let entry = convert_ps_export_to_entry(export);
        assert_eq!(entry.text, "Get-Process");
        assert_eq!(entry.children.len(), 3);

        let id_param = &entry.children[0];
        assert_eq!(id_param.text, "-Id");
        assert_eq!(id_param.label, "-Id (-PID, -ProcessId)");
        assert_eq!(id_param.kind, EntryKind::Param);
        assert_eq!(
            id_param.aliases,
            vec!["PID".to_string(), "ProcessId".to_string()]
        );
        assert_eq!(id_param.parameter_sets, vec!["Id".to_string()]);

        let force_flag = &entry.children[1];
        assert_eq!(force_flag.text, "-Force");
        assert_eq!(force_flag.kind, EntryKind::Flag);
        assert!(force_flag.children[0].is_placeholder());
        assert_eq!(force_flag.children[0].role, EntryRole::Placeholder);

        let ia_param = &entry.children[2];
        assert_eq!(ia_param.text, "-InformationAction");
        assert_eq!(ia_param.label, "-InformationAction (-ia)");
        assert_eq!(ia_param.aliases, vec!["ia".to_string()]);
    }

    #[test]
    fn test_rich_clr_metadata_completions_range_pipeline_and_mandatory_priority() {
        // Backward-compatible legacy JSON without new fields still parses
        let legacy_json = br#"[{"name":"Get-Item","kind":"Cmdlet","desc":"Module: Mgmt","params":[{"name":"Path","type_name":"String[]","is_switch":false,"is_common":false}]}]"#;
        let legacy_entries = parse_ps_metadata_entries(legacy_json);
        assert_eq!(legacy_entries.len(), 1);
        assert_eq!(legacy_entries[0].children[0].text, "-Path");
        assert!(legacy_entries[0].children[0].mandatory_sets.is_empty());

        let rich_json = serde_json::json!([
            {
                "name": "Invoke-Sample",
                "kind": "Cmdlet",
                "desc": "Module: Sample.Mod",
                "help_file": "Sample.Mod.dll-Help.xml",
                "params": [
                    {
                        "name": "Verbose",
                        "type_name": "SwitchParameter",
                        "is_switch": true,
                        "is_common": true,
                        "parameter_sets": ["__AllParameterSets"]
                    },
                    {
                        "name": "OptionalTag",
                        "type_name": "String",
                        "is_switch": false,
                        "is_common": false,
                        "valid_values": ["alpha"],
                        "argument_completions": ["alpha", "beta", "two words"],
                        "parameter_sets": ["__AllParameterSets"]
                    },
                    {
                        "name": "Count",
                        "type_name": "Int32",
                        "is_switch": false,
                        "is_common": false,
                        "parameter_sets": ["__AllParameterSets"],
                        "mandatory_sets": ["__AllParameterSets"],
                        "position": 0,
                        "accepts_pipeline": true,
                        "accepts_pipeline_by_property_name": true,
                        "validate_range": "1..100"
                    }
                ]
            }
        ]);
        let entries = parse_ps_metadata_entries(rich_json.to_string().as_bytes());
        assert_eq!(entries.len(), 1);
        let cmd = &entries[0];
        assert_eq!(cmd.help_file.as_deref(), Some("Sample.Mod.dll-Help.xml"));

        // Mandatory -Count sorts ahead of optional non-common -OptionalTag and common -Verbose
        let names: Vec<&str> = cmd.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["-Count", "-OptionalTag", "-Verbose"]);

        let count = &cmd.children[0];
        assert_eq!(count.mandatory_sets, vec!["__AllParameterSets"]);
        assert_eq!(count.position, Some(0));
        assert!(count.accepts_pipeline);
        assert!(count.accepts_pipeline_by_property_name);
        assert_eq!(count.validate_range.as_deref(), Some("1..100"));
        assert!(count.desc.contains("Pos: 0"));
        assert!(count.desc.contains("Range: 1..100"));

        // ArgumentCompletions merged without duplicating valid_values and multi-word quoted safely
        let opt = &cmd.children[1];
        let val_texts: Vec<&str> = opt.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(val_texts, vec!["alpha", "beta", "\"two words\""]);
        assert_eq!(opt.children[0].kind, EntryKind::Enum);
        assert_eq!(opt.children[1].kind, EntryKind::Value);
    }

    #[test]
    fn test_resolve_powershell_executable_windows_51_fallback_and_profile_temp_file_sync() {
        let win_pwsh = resolve_powershell_executable_with(true, |name| match name {
            "pwsh" => Some(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")),
            "powershell.exe" => Some(PathBuf::from(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            )),
            _ => None,
        });
        assert_eq!(
            win_pwsh,
            Some(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"))
        );

        let win_fallback = resolve_powershell_executable_with(true, |name| match name {
            "powershell.exe" => Some(PathBuf::from(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            )),
            _ => None,
        });
        assert_eq!(
            win_fallback,
            Some(PathBuf::from(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
            ))
        );

        let unix_no_fallback = resolve_powershell_executable_with(false, |name| match name {
            "powershell.exe" => Some(PathBuf::from("/fake/powershell.exe")),
            _ => None,
        });
        assert_eq!(unix_no_fallback, None);

        // Profile sync writes JSON to temp file and ignores noisy stdout from $PROFILE
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("zpane_ps_sync_{}_{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("ps_metadata.json");

        let count_profile = sync_powershell_metadata_with_boundary(
            true,
            true,
            &dest,
            &dir,
            |name| (name == "powershell.exe").then(|| PathBuf::from("C:\\fake\\powershell.exe")),
            |exe, args, out_file| {
                assert_eq!(exe, Path::new("C:\\fake\\powershell.exe"));
                assert!(!args.contains(&"-NoProfile"));
                let out_path = out_file.expect("sync_profile=true must pass temp file");
                std::fs::write(
                    out_path,
                    br#"[{"name":"My-ProfileFunc","kind":"Func","desc":"Session","params":[]}]"#,
                )
                .unwrap();
                // Return noisy profile stdout to prove temp file bypasses stdout
                Ok(b"Welcome to PowerShell!\n\x1b[32mOhMyPosh\x1b[0m\n".to_vec())
            },
        )
        .expect("profile sync succeeds");
        assert_eq!(count_profile, 1);
        let saved = std::fs::read(&dest).unwrap();
        let parsed = parse_ps_metadata_entries(&saved);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "My-ProfileFunc");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_maml_xml_help_parser_and_bounded_directory_lookup_sanitize_controls() {
        let maml = r#"<?xml version="1.0" encoding="utf-8"?>
<helpItems schema="maml" xmlns="http://msh">
  <command:command xmlns:maml="http://schemas.microsoft.com/maml/2004/10" xmlns:command="http://schemas.microsoft.com/maml/dev/command/2004/10">
    <command:details>
      <command:name>Start-ThreadJob</command:name>
      <maml:description>
        <maml:para>Starts a PowerShell job &amp; runs in a separate thread.</maml:para>
      </maml:description>
    </command:details>
    <maml:description>
      <maml:para>Detailed docs line 1.</maml:para>
      <maml:para>Detailed docs line 2 with &#x1b;[31mred&#x1b;[0m and &lt;tag&gt;.</maml:para>
    </maml:description>
    <command:parameters>
      <command:parameter required="true" position="0">
        <maml:name>FilePath</maml:name>
        <maml:description>
          <maml:para>Specifies the script file path.</maml:para>
        </maml:description>
      </command:parameter>
      <command:parameter required="false" position="named">
        <maml:name>ThrottleLimit</maml:name>
        <maml:description>
          <maml:para>Maximum concurrent jobs.</maml:para>
        </maml:description>
      </command:parameter>
    </command:parameters>
  </command:command>
</helpItems>"#;

        let map = parse_maml_help_xml(maml);
        let help = map.get("start-threadjob").expect("Start-ThreadJob parsed");
        assert_eq!(
            help.synopsis,
            "Starts a PowerShell job & runs in a separate thread."
        );
        assert!(help.description.contains("Detailed docs line 1."));
        assert!(help.description.contains("<tag>"));
        assert!(!help.description.contains('\x1b'));
        assert_eq!(
            help.params.get("filepath").map(String::as_str),
            Some("Specifies the script file path.")
        );
        assert_eq!(
            help.params.get("throttlelimit").map(String::as_str),
            Some("Maximum concurrent jobs.")
        );

        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("zpane_maml_test_{}_{uniq}", std::process::id()));
        let mod_dir = root.join("Microsoft.PowerShell.ThreadJob").join("en-US");
        std::fs::create_dir_all(&mod_dir).unwrap();
        std::fs::write(
            mod_dir.join("Microsoft.PowerShell.ThreadJob.dll-Help.xml"),
            maml,
        )
        .unwrap();

        let (found, _path) = lookup_ps_maml_help_in_dirs(
            "Start-ThreadJob",
            Some("Microsoft.PowerShell.ThreadJob.dll-Help.xml"),
            std::slice::from_ref(&root),
        )
        .expect("MAML file found in fixture dir");
        assert_eq!(found.synopsis, help.synopsis);

        let mut cmd_entry = Entry::new(
            "Start-ThreadJob",
            "Start-ThreadJob",
            EntryKind::Cmdlet,
            "Module: Microsoft.PowerShell.ThreadJob",
        );
        cmd_entry.children = vec![Entry::new(
            "-FilePath",
            "-FilePath",
            EntryKind::Param,
            "[String] Parameter -FilePath",
        )];
        apply_ps_help_to_entry(&mut cmd_entry, &found);
        assert!(cmd_entry.desc.contains("Starts a PowerShell job"));
        assert!(cmd_entry.desc.contains("Detailed docs line 1."));
        assert_eq!(
            cmd_entry.children[0].desc,
            "[String] Specifies the script file path."
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
