use super::catalog::{cache_dir, hide_window, is_safe_command_token, write_secure_file};
use super::introspect::find_executable_in_path;
use crate::model::{Entry, EntryKind};
use crate::sanitize::sanitize_single_line;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub(super) const MAX_PS_METADATA_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PsParamExport {
    pub(super) name: String,
    #[serde(default)]
    pub(super) aliases: Vec<String>,
    pub(super) type_name: String,
    pub(super) is_switch: bool,
    pub(super) is_common: bool,
    #[serde(default)]
    pub(super) valid_values: Vec<String>,
    #[serde(default)]
    pub(super) parameter_sets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PsCommandExport {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) desc: String,
    #[serde(default)]
    pub(super) params: Vec<PsParamExport>,
}

pub(super) fn ps_metadata_path() -> PathBuf {
    cache_dir().join("ps_metadata.json")
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
    let mut cmd_entry = Entry::new(cmd_name, cmd_name, kind, cmd_desc.as_ref());
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
            .into_iter()
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
        let desc = format!("[{p_type}] Parameter {label}");

        let mut param_entry = Entry::new(&p_flag, label, p_kind, desc)
            .with_parameter_sets(p.parameter_sets)
            .with_aliases(clean_aliases);

        param_entry
            .children
            .reserve(p.valid_values.len() + if p.is_switch { 3 } else { 0 });
        for val in p.valid_values {
            let clean_val = sanitize_single_line(val.trim());
            if !clean_val.is_empty() {
                param_entry.children.push(Entry::new(
                    clean_val.as_ref(),
                    clean_val.as_ref(),
                    EntryKind::Enum,
                    format!("Allowed [{p_type}] value for {p_flag}"),
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

/// Synchronizes PowerShell Cmdlets, Functions, Aliases, and Parameter Metadata to disk.
pub fn sync_powershell_metadata() -> anyhow::Result<usize> {
    let ps_script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$commonSet = [System.Collections.Generic.HashSet[string]]::new(
    [string[]]@('Verbose','Debug','ErrorAction','WarningAction','InformationAction','ProgressAction','ErrorVariable','WarningVariable','InformationVariable','OutVariable','OutBuffer','PipelineVariable','WhatIf','Confirm'),
    [System.StringComparer]::OrdinalIgnoreCase
)
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
    $pList = [System.Collections.Generic.List[object]]::new()
    if ($target.Parameters) {
        foreach ($p in $target.Parameters.Values) {
            $vals = [System.Collections.Generic.List[string]]::new()
            if ($p.ParameterType.IsEnum) {
                foreach ($n in [System.Enum]::GetNames($p.ParameterType)) { $vals.Add($n) }
            }
            foreach ($attr in $p.Attributes) {
                if ($attr -is [System.Management.Automation.ValidateSetAttribute]) {
                    foreach ($v in $attr.ValidValues) { if (-not $vals.Contains($v)) { $vals.Add($v) } }
                }
            }
            $paramSets = [string[]]@($p.ParameterSets.Keys)
            $pList.Add(@{
                name           = $p.Name
                aliases        = @($p.Aliases)
                type_name      = ($p.ParameterType.Name -replace '`1$', '')
                is_switch      = [bool]$p.SwitchParameter
                is_common      = [bool]$commonSet.Contains($p.Name)
                valid_values   = $vals
                parameter_sets = $paramSets
            })
        }
    }
    $out.Add(@{
        name   = $c.Name
        kind   = $kind
        desc   = $desc
        params = $pList
    })
}
if ($out.Count -eq 0) { '[]' } else { ConvertTo-Json -InputObject @($out) -Depth 6 -Compress }
"#;

    let pwsh_path = find_executable_in_path("pwsh").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "pwsh executable not found on PATH",
        )
    })?;
    let mut cmd = Command::new(pwsh_path);
    cmd.args(["-NoLogo", "-NoProfile", "-Command", ps_script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = hide_window(&mut cmd).spawn()?;
    let mut stdout_buf = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_PS_METADATA_BYTES.saturating_add(1))
            .read_to_end(&mut stdout_buf)?;
    }
    let status = child.wait()?;
    if (stdout_buf.len() as u64) > MAX_PS_METADATA_BYTES {
        anyhow::bail!("pwsh metadata export exceeded {MAX_PS_METADATA_BYTES} byte limit");
    }
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "pwsh metadata export failed with status {status}"
        ))
        .into());
    }

    let exports: Vec<PsCommandExport> = serde_json::from_slice(&stdout_buf)?;
    let count = exports.len();
    let dest = ps_metadata_path();
    write_secure_file(&dest, &stdout_buf)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryRole;

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
                },
                PsParamExport {
                    name: "Force".to_string(),
                    aliases: vec!["f".to_string()],
                    type_name: "SwitchParameter".to_string(),
                    is_switch: true,
                    is_common: false,
                    valid_values: Vec::new(),
                    parameter_sets: vec!["__AllParameterSets".to_string()],
                },
                PsParamExport {
                    name: "InformationAction".to_string(),
                    aliases: vec!["ia".to_string()],
                    type_name: "ActionPreference".to_string(),
                    is_switch: false,
                    is_common: true,
                    valid_values: vec!["SilentlyContinue".to_string(), "Continue".to_string()],
                    parameter_sets: vec!["__AllParameterSets".to_string()],
                },
            ],
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
}
