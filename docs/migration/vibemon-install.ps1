# Draft of the next `vibemon.dev/install.ps1`: installs AttemptDB, replaces
# the VibeMon legacy hooks with `attempt hook`, and links this device to the
# hosted VibeMon sync server. Windows counterpart of vibemon-install.sh.
#
#   irm https://vibemon.dev/install.ps1 | iex
#   # or, with arguments:
#   & ([scriptblock]::Create((irm https://vibemon.dev/install.ps1))) -Key atk_...
#
# Parameters
#   -Key KEY          device key issued by VibeMon (required on first run)
#   -Server URL       sync server (default: the `vibemon` alias)
#   -Profile NAME     metadata_only | semantic | full (default: semantic)
#   -LocalContent     keep prompts / commands / tool output in the LOCAL
#                     encrypted database; off by default so an existing
#                     VibeMon user's metadata-only promise holds on disk
#                     until they choose otherwise
#   -KeepLegacy       leave the ~/.vibemon hook entries in place
#   -DryRun           print the commands instead of running them
#
# Known gap (TODO.md §21.4): the background daemon is not implemented on
# Windows yet, so this script registers a per-user Scheduled Task that runs
# `attempt import` (spool → database) and `attempt sync now` every five
# minutes instead. Hooks themselves never wait on it: they append to the
# spool and exit.
[CmdletBinding()]
param(
    [string]$Key = "",
    [string]$Server = "vibemon",
    [ValidateSet("metadata_only", "semantic", "full")]
    [string]$Profile = "semantic",
    [switch]$LocalContent,
    [switch]$KeepLegacy,
    [switch]$DryRun
)
$ErrorActionPreference = "Stop"
$Installer = "https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1"
$BinDir = if ($env:ATTEMPTDB_BIN_DIR) { $env:ATTEMPTDB_BIN_DIR } else { Join-Path $env:LOCALAPPDATA "AttemptDB\bin" }

function Invoke-Step {
    param([string[]]$Cmd)
    if ($DryRun) { Write-Host ("+ " + ($Cmd -join " ")); return }
    & $Cmd[0] @($Cmd[1..($Cmd.Length - 1)])
    if ($LASTEXITCODE -ne 0) { throw "failed: $($Cmd -join ' ')" }
}

# 1. The binary (install.ps1 verifies SHA256SUMS and never touches agent config).
if ($DryRun) {
    Write-Host "+ irm $Installer | iex"
} else {
    Invoke-Expression (Invoke-RestMethod $Installer)
}
if (-not ($env:PATH -split ";" | Where-Object { $_ -eq $BinDir })) { $env:PATH = "$BinDir;$env:PATH" }
if (-not $DryRun -and -not (Get-Command attempt -ErrorAction SilentlyContinue)) {
    throw "attempt is not on PATH after install; add $BinDir to PATH and re-run"
}

# 2. The local database (no-op when it exists); metadata-only on disk unless
#    the user opted into local content.
$captureMode = if ($LocalContent) { "local_semantic" } else { "metadata_only" }
Invoke-Step @("attempt", "init", "--capture-mode", $captureMode, "--source", "vibemon")

# 3. Hooks: ours in, the legacy notify.sh entries out.
if ($KeepLegacy) {
    Invoke-Step @("attempt", "hook", "install")
} else {
    Invoke-Step @("attempt", "hook", "install", "--remove-legacy", "vibemon")
}

# 4. Link the device. A key is needed once; later runs keep the connection.
$connected = $false
if (-not $DryRun) {
    try {
        $status = attempt sync status --json | ConvertFrom-Json
        $connected = [bool]$status.connected
    } catch { $connected = $false }
}
if ($Key -ne "") {
    Invoke-Step @("attempt", "sync", "connect", $Server, "--key", $Key, "--profile", $Profile)
} elseif (-not $connected -and -not $DryRun) {
    throw "no -Key given and this device is not linked yet; get a key at https://vibemon.dev/devices and re-run with -Key"
}

# 5. Uploads: a Scheduled Task stands in for the daemon on Windows.
$attemptExe = if ($DryRun) { "attempt.exe" } else { (Get-Command attempt).Source }
$taskName = "AttemptDB Sync"
$taskCmd = "`"$attemptExe`" import; `"$attemptExe`" sync now"
$action = "powershell.exe -NoProfile -WindowStyle Hidden -Command `"$taskCmd`""
if ($DryRun) {
    Write-Host "+ schtasks /Create /F /SC MINUTE /MO 5 /TN `"$taskName`" /TR '$action'"
} else {
    schtasks /Create /F /SC MINUTE /MO 5 /TN "$taskName" /TR $action | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "could not register the '$taskName' scheduled task" }
}

# 6. First upload now, so the device shows up on the web immediately.
Invoke-Step @("attempt", "sync", "now")

# 7. What the user sees: agents detected, hooks installed, database, sync.
Write-Host ""
Invoke-Step @("attempt", "doctor")
Write-Host ""
Write-Host "done. Open https://vibemon.dev to see this device; run 'attempt sync status' any time to see what has been uploaded."
if (-not $KeepLegacy -and (Test-Path (Join-Path $HOME ".vibemon"))) {
    Write-Host "legacy client left at ~\.vibemon (not referenced by the hooks any more); remove it with: Remove-Item -Recurse ~\.vibemon"
}
