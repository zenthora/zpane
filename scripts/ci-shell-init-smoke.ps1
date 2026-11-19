#!/usr/bin/env pwsh
# Parses `zpane init pwsh` output with the PowerShell AST parser without executing it.
# Usage: pwsh -File scripts/ci-shell-init-smoke.ps1 -ZpaneBinary <path-to-zpane>
param(
    [Parameter(Mandatory = $true)]
    [string]$ZpaneBinary
)

$ErrorActionPreference = 'Stop'

$script = (& $ZpaneBinary init pwsh | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "'zpane init pwsh' exited with code $LASTEXITCODE"
}
if ([string]::IsNullOrWhiteSpace($script)) {
    throw "'zpane init pwsh' produced no output"
}

$tokens = $null
$parseErrors = $null
[void][System.Management.Automation.Language.Parser]::ParseInput($script, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -gt 0) {
    foreach ($e in $parseErrors) {
        Write-Error ("{0} (line {1}, column {2})" -f $e.Message, $e.Extent.StartLineNumber, $e.Extent.StartColumnNumber) -ErrorAction Continue
    }
    exit 1
}
Write-Host 'ok: zpane init pwsh parses cleanly'
