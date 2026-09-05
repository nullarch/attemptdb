# vibemon.dev's one-line install for Windows: AttemptDB on this machine,
# linked to the VibeMon sync server with a one-time pairing token from the
# web. Counterpart of vibemon-install.sh, same order, same safety:
#
#   & ([scriptblock]::Create((irm https://vibemon.dev/install.ps1))) -Pair pair_abc123
#
#   1. checks the pairing token with the server before touching anything;
#      no token (or a dead one) -> nothing on this machine changes, exit 0
#   2. installs (or upgrades) `attempt`, verified against SHA256SUMS
#   3. creates the local database if there is none (an existing one keeps
#      its capture mode and settings)
#   4. pairs: token + this database's device id -> a device key, proven by an
#      authenticated handshake, saved only on success
#   5. installs the agent hooks next to any existing ones
#   6. registers the per-user Scheduled Task (`attempt daemon install`) that
#      uploads every minute — opening the database imports whatever the
#      hooks spooled, so one command does both (the Windows daemon is not
#      implemented yet; hooks never wait on it, they append and exit)
#   7. uploads once and requires the server to accept it
#   8. only then removes the legacy VibeMon hooks (~\.vibemon\notify.py)
#   9. shows `attempt doctor`
#
# Parameters
#   -Pair TOKEN       one-time pairing token from https://vibemon.dev/devices
#   -ApiKey vbm_KEY   the account API key from the older install command:
#                     exchanged for a pairing token at the web first
#   -Web URL          the product web (default: https://vibemon.dev)
#   -Server URL       sync server (default https://sync.vibemon.dev or $env:VIBEMON_SYNC_URL)
#   -Profile NAME     metadata_only | semantic | full (default semantic)
#   -LocalContent     keep prompts / commands / tool output in the LOCAL
#                     encrypted database on a NEW install (off by default)
#   -KeepLegacy       leave the legacy hook entries in place
#   -DryRun           print the commands instead of running them
#   -NoReport         do not tell vibemon.dev how this run ended. By default
#                     one line goes back when the script exits — ok or failed,
#                     the step it stopped at, versions, and the account key if
#                     it was used (resolved on the web, never stored) — so a
#                     failure on a machine nobody is watching is still seen.
#                     Never paths, never hostnames.
#   -NoCommitMsg      the older client's flag; accepted and ignored
[CmdletBinding()]
param(
    [string]$Pair = "",
    # The account API key from the older install command (vbm_...): exchanged
    # for a pairing token at the web before anything changes.
    [string]$ApiKey = "",
    [string]$Web = "",
    [string]$Server = "",
    [ValidateSet("metadata_only", "semantic", "full")]
    [string]$Profile = "semantic",
    [switch]$LocalContent,
    [switch]$KeepLegacy,
    [switch]$DryRun,
    [switch]$NoReport,
    # The older client's command carried this; nothing here reads it.
    [switch]$NoCommitMsg
)
$ErrorActionPreference = "Stop"
$DefaultServer = if ($env:VIBEMON_SYNC_URL) { $env:VIBEMON_SYNC_URL } else { "https://sync.vibemon.dev" }
if ($Server -eq "") { $Server = $DefaultServer }
$Server = $Server.TrimEnd("/")
# The AttemptDB release this script was written against, pinned; the binary
# installer comes from the same tag. -Pair needs 0.2.0 or later. A newer
# `attempt` already on the machine is kept.
$AttemptVersion = if ($env:ATTEMPTDB_VERSION) { $env:ATTEMPTDB_VERSION } else { "0.2.7" }
$env:ATTEMPTDB_VERSION = $AttemptVersion
$Installer = if ($env:ATTEMPTDB_INSTALLER) { $env:ATTEMPTDB_INSTALLER } else { "https://raw.githubusercontent.com/nullarch/attemptdb/v$AttemptVersion/install.ps1" }
$BinDir = if ($env:ATTEMPTDB_BIN_DIR) { $env:ATTEMPTDB_BIN_DIR } else { Join-Path $env:LOCALAPPDATA "AttemptDB\bin" }

function Invoke-Step {
    param([string[]]$Cmd)
    if ($DryRun) { Write-Host ("+ " + ($Cmd -join " ")); return $true }
    & $Cmd[0] @($Cmd[1..($Cmd.Length - 1)])
    return ($LASTEXITCODE -eq 0)
}
$Step = "start"
$script:LastError = ""
$script:Reported = $false
# The older client's daily poll runs this detached with every stream on
# NUL; a person at a console has a live stdout. That is the whole test.
$Unattended = $false
try { $Unattended = [Console]::IsOutputRedirected } catch { $Unattended = $false }
# One line back to the web when this script ends, however it ends (see
# -NoReport). Best effort: five seconds, never a failure of its own.
function Send-Report {
    param([bool]$Ok)
    if ($NoReport -or $DryRun -or $script:Reported) { return }
    $script:Reported = $true
    $av = ""
    try { $out = (& attempt --version 2>$null); if ($out -match '(\d+\.\d+\.\d+)') { $av = $Matches[1] } } catch {}
    $err = ""
    if ($script:LastError) { $err = ([string]$script:LastError -split "`n")[0]; if ($err.Length -gt 300) { $err = $err.Substring(0, 300) } }
    $body = @{ ok = $Ok; step = $Step; os = "Windows"; arch = [string]$env:PROCESSOR_ARCHITECTURE; installer_version = $AttemptVersion; attempt_version = $av; unattended = $Unattended; error = $err; api_key = $ApiKey } | ConvertTo-Json -Compress
    try { Invoke-RestMethod -Method Post -Uri "$Web/api/attemptdb/install-report" -ContentType "application/json" -Body $body -TimeoutSec 5 | Out-Null } catch {}
}
function Fail { param([string]$Message) $script:LastError = $Message; Send-Report $false; Write-Error "vibemon: $Message"; exit 1 }

if (-not ($env:PATH -split ";" | Where-Object { $_ -eq $BinDir })) { $env:PATH = "$BinDir;$env:PATH" }
$connected = $false
if (-not $DryRun -and (Get-Command attempt -ErrorAction SilentlyContinue)) {
    try { $connected = [bool]((attempt sync status --json | ConvertFrom-Json).connected) } catch { $connected = $false }
}

if ($Web -eq "") { $Web = if ($env:VIBEMON_WEB_URL) { $env:VIBEMON_WEB_URL } else { "https://vibemon.dev" } }
$Web = $Web.TrimEnd("/")

# 0a. No argument, a legacy install on this machine: the older client kept
#     the account key in ~\.vibemon\api-key. Use it — typed by a person (the
#     app's "update available" command) or run unattended by the older
#     client's poll when the web tells it to (install.sh?v changed). The same
#     safety applies: nothing is removed until an upload succeeded, and a
#     failure is reported.
$Step = "pair"
if ($Pair -eq "" -and $ApiKey -eq "" -and -not $connected) {
    $keyFile = Join-Path $HOME ".vibemon\api-key"
    if (Test-Path $keyFile) {
        try {
            $m = [regex]::Match((Get-Content $keyFile -Raw), 'vbm_[A-Za-z0-9_-]+')
            if ($m.Success) {
                $ApiKey = $m.Value
                Write-Host "vibemon: found the account key of the older client in ~\.vibemon\api-key; upgrading this machine to AttemptDB"
            }
        } catch {}
    }
}

# 0. A legacy API key becomes a pairing token at the web (server side; the
#    key is looked up there and goes nowhere else). Before anything changes.
if ($ApiKey -ne "" -and $Pair -eq "") {
    if (-not $ApiKey.StartsWith("vbm_")) { Fail "$ApiKey is not an API key (vbm_...)" }
    if ($DryRun) {
        Write-Host "+ POST $Web/api/attemptdb/pair  (vbm_... -> pair_...)"
        $Pair = "pair_dryrun"
    } else {
        try {
            $r = Invoke-RestMethod -Method Post -Uri "$Web/api/attemptdb/pair" -ContentType "application/json" -Body (@{ api_key = $ApiKey } | ConvertTo-Json) -TimeoutSec 20
            $Pair = [string]$r.token
            # The web knows where its sync server is; an explicit -Server still wins.
            if ($Server -eq $DefaultServer.TrimEnd("/") -and $r.sync_url) { $Server = ([string]$r.sync_url).TrimEnd("/") }
        } catch {
            Fail "the web did not accept this API key: $($_.Exception.Message) (nothing changed; get a command at $Web/devices)"
        }
        if (-not $Pair.StartsWith("pair_")) { Fail "the web did not return a pairing token (nothing changed)" }
    }
}

# 1. The gate: no token and never connected is the legacy client polling
#    for updates, not an install. Nothing changes.
if ($Pair -eq "" -and -not $connected) {
    $Step = "noop"
    Write-Host "vibemon: no pairing token given and this machine is not connected; nothing changed."
    Write-Host "         get a one-line command at https://vibemon.dev/devices"
    Send-Report $true
    exit 0
}
if ($Pair -ne "") {
    if (-not $Pair.StartsWith("pair_")) { Fail "$Pair is not a pairing token (pair_...)" }
    if ($DryRun) {
        Write-Host "+ GET $Server/v1/pair/$Pair"
    } else {
        try {
            Invoke-RestMethod -Method Get -Uri "$Server/v1/pair/$Pair" -TimeoutSec 20 | Out-Null
        } catch {
            $code = 0
            try { $code = [int]$_.Exception.Response.StatusCode } catch {}
            switch ($code) {
                410 { Fail "the pairing token has expired or was already used; get a new one at https://vibemon.dev/devices" }
                404 { Fail "the server does not know this pairing token; get a new one at https://vibemon.dev/devices" }
                0   { Fail "cannot reach $Server; check the network and try again (nothing changed)" }
                default { Fail "the server answered $code to the pairing check (nothing changed)" }
            }
        }
    }
}

$Step = "binary"
# 2. The binary (install.ps1 verifies SHA256SUMS and never touches agent
#    config). Skipped when the machine already has the pinned version or newer.
$present = $null
$cmd = Get-Command attempt -ErrorAction SilentlyContinue
if ($cmd) {
    $out = (& attempt --version 2>$null)
    if ($out -match '(\d+\.\d+\.\d+)') { try { $present = [version]$Matches[1] } catch { $present = $null } }
}
if ($present -and $present -ge [version]$AttemptVersion) {
    Write-Host "attempt $present present (need $AttemptVersion or newer); keeping it"
} elseif ($DryRun) {
    Write-Host "+ `$env:ATTEMPTDB_VERSION=$AttemptVersion; irm $Installer | iex"
} else {
    if ($present) { Write-Host "attempt $present present; installing $AttemptVersion" }
    Invoke-Expression (Invoke-RestMethod $Installer)
    if (-not (Get-Command attempt -ErrorAction SilentlyContinue)) { Fail "attempt is not on PATH after install; add $BinDir to PATH and re-run" }
}

$Step = "init"
# 3. The local database; an existing one is left as it is.
$exists = $false
if (-not $DryRun) { try { attempt status *> $null; $exists = ($LASTEXITCODE -eq 0) } catch { $exists = $false } }
if ($exists) {
    if (-not (Invoke-Step @("attempt", "init", "--source", "vibemon"))) { Fail "attempt init failed" }
} else {
    $mode = if ($LocalContent) { "local_semantic" } else { "metadata_only" }
    if (-not (Invoke-Step @("attempt", "init", "--capture-mode", $mode, "--source", "vibemon"))) { Fail "attempt init failed" }
}

$Step = "connect"
# 4. Pairing: the key is saved only after the server accepted this device.
if ($Pair -ne "") {
    $label = try { [System.Net.Dns]::GetHostName() } catch { "device" }
    if (-not (Invoke-Step @("attempt", "sync", "connect", $Server, "--pair", $Pair, "--profile", $Profile, "--label", $label))) {
        Fail "pairing failed; nothing else was changed - fix the cause and run the command again with a fresh token"
    }
}

$Step = "hooks"
# 5. Hooks, next to whatever is there.
if (-not (Invoke-Step @("attempt", "hook", "install"))) { Fail "hook install failed" }

$Step = "daemon"
# 6. Uploads: a Scheduled Task stands in for the daemon on Windows, and
#    `attempt daemon install` owns it — registering it here by hand is how
#    the first version shipped a task whose PowerShell one-liner depended on
#    how -Command stripped quotes. The CLI registers the executable itself,
#    with its arguments, which nothing has to re-parse; `attempt daemon
#    uninstall` (and `attempt uninstall`) remove it again.
if (-not (Invoke-Step @("attempt", "daemon", "install"))) {
    Fail "could not register the 'AttemptDB Sync' scheduled task (attempt daemon install)"
}

$Step = "upload"
# 7. One upload now; the server must accept it before anything is removed.
if (-not (Invoke-Step @("attempt", "sync", "now"))) {
    Write-Host ""
    Write-Host "vibemon: the first upload did not go through. AttemptDB is installed and hooks are in place,"
    Write-Host "         but the legacy VibeMon hooks were left untouched so collection continues as before."
    Write-Host "         Run 'attempt sync status' for the error, then 'attempt sync now'; once it succeeds,"
    Write-Host "         re-run this command to finish the switch."
    $script:LastError = "the first upload did not go through"
    Send-Report $false
    exit 1
}

$Step = "remove_legacy"
# 8. The legacy client's hook entries - only now.
if (-not $KeepLegacy) {
    if (-not (Invoke-Step @("attempt", "hook", "install", "--remove-legacy", "vibemon"))) { Fail "removing the legacy hooks failed" }
    if (Test-Path (Join-Path $HOME ".vibemon")) {
        Write-Host "legacy client left at ~\.vibemon (no hook references it any more); remove it with: Remove-Item -Recurse ~\.vibemon"
    }
}

$Step = "done"
# 9. What the user sees.
Write-Host ""
Invoke-Step @("attempt", "doctor") | Out-Null
Write-Host ""
Write-Host "done. https://vibemon.dev/devices shows this device; 'attempt sync status' shows what left this machine."
Send-Report $true
