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
#      its capture mode and settings: this script never raises it; only an
#      explicit -LocalContent / -MetadataOnly or $env:VIBEMON_CAPTURE_MODE
#      changes it) and says which mode is in effect
#   4. pairs: token + this database's device id -> a device key, proven by an
#      authenticated handshake, saved only on success
#   5. installs the agent hooks next to any existing ones
#   6. registers the per-user Scheduled Task (`attempt daemon install`) that
#      keeps capture, automatic sync, and the local OTel receiver running
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
#   -Profile NAME     metadata_only | semantic | messages | full (default
#                     messages: metadata, inferences, and the conversation —
#                     your prompts and the agent's messages, secrets redacted;
#                     commands and tool output stay on this machine)
#   -LocalContent     set the capture mode to local_semantic, on a new or an
#                     existing database: prompts, commands and tool output are
#                     kept in the LOCAL encrypted database
#   -MetadataOnly     set the capture mode to metadata_only (no content stored)
#   $env:VIBEMON_CAPTURE_MODE
#                     metadata_only | local_semantic | full_sync, the same as
#                     the two switches (they win). Without any of the three a
#                     NEW database is created local_semantic and an existing
#                     one is left exactly as it is, metadata_only included
#   -KeepLegacy       leave the legacy hook entries in place
#   -DryRun           print the commands instead of running them
#   -NoReport         do not tell vibemon.dev how this run ended. By default
#                     one line goes back when the script exits — ok or failed,
#                     the step it stopped at, versions, the account key if it
#                     was used (resolved on the web, never stored), and the
#                     last 40 lines of the run's transcript with home paths
#                     and keys blanked - attended or not, so a failure is
#                     always one somebody can read. A failed service step
#                     ends the transcript with facts about the machine
#                     (Windows build, PowerShell, admin or not, the task).
#                     Every run logs to ~\.vibemon\vibemon-install.log (or
#                     %LOCALAPPDATA%\AttemptDB\state); a dry run logs nothing.
#   -NoCommitMsg      the older client's flag; accepted and ignored
[CmdletBinding()]
param(
    [string]$Pair = "",
    # The account API key from the older install command (vbm_...): exchanged
    # for a pairing token at the web before anything changes.
    [string]$ApiKey = "",
    [string]$Web = "",
    [string]$Server = "",
    [ValidateSet("metadata_only", "semantic", "messages", "full")]
    [string]$Profile = "messages",
    [switch]$LocalContent,
    [switch]$MetadataOnly,
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
# 0.2.15 knows the `messages` sync profile the connect step asks for; an
# older binary rejects `-Profile messages` and pairing fails. tests/installers
# pins this to the workspace version. A newer `attempt` already on the
# machine is kept.
$AttemptVersion = if ($env:ATTEMPTDB_VERSION) { $env:ATTEMPTDB_VERSION } else { "0.2.15" }
$InstallerVersion = "0.2.15+install.1"
$env:ATTEMPTDB_VERSION = $AttemptVersion
$Installer = if ($env:ATTEMPTDB_INSTALLER) { $env:ATTEMPTDB_INSTALLER } else { "https://raw.githubusercontent.com/nullarch/attemptdb/v$AttemptVersion/install.ps1" }
$BinDir = if ($env:ATTEMPTDB_BIN_DIR) { $env:ATTEMPTDB_BIN_DIR } else { Join-Path $env:LOCALAPPDATA "AttemptDB\bin" }

function Invoke-Step {
    param([string[]]$Cmd)
    if ($DryRun) { Write-Host ("+ " + ($Cmd -join " ")); return $true }
    # Native stdout must not join the boolean return value: an array of
    # output lines plus $false is truthy and would bypass failure gates.
    & $Cmd[0] @($Cmd[1..($Cmd.Length - 1)]) | Out-Host
    return ($LASTEXITCODE -eq 0)
}
$Step = "start"
$script:LastError = ""
$script:Reported = $false
# The older client's daily poll runs this detached with every stream on
# NUL; a person at a console has a live stdout. That is the whole test.
$Unattended = $false
try { $Unattended = [Console]::IsOutputRedirected } catch { $Unattended = $false }
# The run log: a transcript, for every run, and the report carries its tail.
# Unattended, every stream is NUL and the transcript is the only record. An
# attended run used to keep none, so a failure on a person's own terminal
# reported nothing; a transcript shows the console exactly as it was. A dry
# run of an attended person writes nothing (it prints the pairing token). A
# transcript this script did not start is never stopped, and one this script
# started is always stopped (Stop-InstallTranscript, on every way out): the
# attended command runs in the person's own session.
$script:Log = ""
$script:TranscriptOn = $false
if ($Unattended -or -not $DryRun) {
    # Next to the older client when there is one (never created for it),
    # else our own state directory.
    $legacyDir = Join-Path $HOME ".vibemon"
    if (Test-Path $legacyDir) { $script:Log = Join-Path $legacyDir "vibemon-install.log" }
    else {
        $d = Join-Path $env:LOCALAPPDATA "AttemptDB\state"
        try { New-Item -ItemType Directory -Force -Path $d | Out-Null; $script:Log = Join-Path $d "vibemon-install.log" } catch {}
    }
    if ($script:Log) {
        try { Start-Transcript -Path $script:Log -Append | Out-Null; $script:TranscriptOn = $true } catch {
            # Git Bash can already hold the parent install log open. Keep a
            # separate transcript rather than silently losing the diagnostics.
            $script:Log = Join-Path (Split-Path $script:Log) "vibemon-install-powershell.log"
            try { Start-Transcript -Path $script:Log -Append | Out-Null; $script:TranscriptOn = $true } catch { $script:Log = "" }
        }
    }
}
function Stop-InstallTranscript {
    if ($script:TranscriptOn) {
        $script:TranscriptOn = $false
        try { Stop-Transcript | Out-Null } catch {}
    }
}
# What explains a failed service step, as facts about the machine and nothing
# else (no file contents, no secrets). Written to the console, so the
# transcript ends with it and the report's tail keeps it.
function Write-Fingerprint {
    if ($DryRun -or ($Step -ne "daemon" -and $Step -ne "environment")) { return }
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        Write-Host "--- environment (facts only; for support) ---"
        $admin = $false
        try { $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) } catch {}
        Write-Host ("step=" + $Step + " os=Windows " + [Environment]::OSVersion.Version + " arch=" + $env:PROCESSOR_ARCHITECTURE + " ps=" + $PSVersionTable.PSVersion + " admin=" + $admin)
        $task = "unavailable"
        try { $task = ((& schtasks.exe /Query /TN "AttemptDB Sync" 2>&1 | Select-Object -First 3) -join " | ") } catch {}
        Write-Host ("task=" + $task)
        $policy = ""
        try { $policy = [string](Get-ExecutionPolicy) } catch {}
        Write-Host ("execution_policy=" + $policy + " language_mode=" + $ExecutionContext.SessionState.LanguageMode)
    } catch {} finally { $ErrorActionPreference = $previous }
}
# One line back to the web when this script ends, however it ends (see
# -NoReport). Best effort: five seconds, never a failure of its own.
function Protect-Diagnostic {
    param([string]$Text)
    # A rejected VibeMon credential may still be a secret for another service.
    foreach ($credential in @($ApiKey, $Pair)) {
        if ($credential -and $credential.Length -ge 4) {
            $Text = $Text.Replace($credential, '[redacted credential]')
        }
    }
    return $Text -replace '(vbm|pair|atk)_[A-Za-z0-9_-]+', '$1_[redacted]' -replace 'Bearer\s+[^\s"'']+', 'Bearer [redacted]' -replace '[A-Za-z]:\\[^\s"]*', '[path]' -replace '/(Users|home|private|tmp|var|root|opt|mnt)/[^\s"]*', '[path]'
}
function Send-Report {
    param([bool]$Ok)
    if ($NoReport -or $DryRun -or $script:Reported) { Stop-InstallTranscript; return }
    $script:Reported = $true
    $av = ""
    try { $out = (& attempt --version 2>$null); if ($out -match '(\d+\.\d+\.\d+)') { $av = $Matches[1] } } catch {}
    $err = ""
    if ($script:LastError) {
        $err = ((Protect-Diagnostic ([string]$script:LastError)) -split "`n")[0]
        if ($err.Length -gt 300) { $err = $err.Substring(0, 300) }
    }
    # The transcript's tail, made safe for a report: keys and tokens blanked,
    # home and temp paths blanked, at most ~4 KB.
    $tail = ""
    if ($script:Log -and (Test-Path $script:Log)) {
        try {
            Stop-InstallTranscript
            $lines = Get-Content $script:Log -Tail 40 -ErrorAction SilentlyContinue
            $tail = (($lines | ForEach-Object { $safe = Protect-Diagnostic ([string]$_); $safe.Substring(0, [Math]::Min(200, $safe.Length)) }) -join "`n")
            if ($tail.Length -gt 4000) { $tail = $tail.Substring($tail.Length - 4000) }
        } catch { $tail = "" }
    }
    $reportKey = if ($ApiKey -cmatch '^vbm_[A-Za-z0-9_-]{8,128}$') { $ApiKey } else { "" }
    $report = @{ ok = $Ok; step = $Step; os = "Windows"; arch = [string]$env:PROCESSOR_ARCHITECTURE; installer_version = $InstallerVersion; attempt_version = $av; unattended = $Unattended; error = $err; api_key = $reportKey; log_tail = $tail }
    $body = $report | ConvertTo-Json -Compress
    # JSON escaping can expand a 4 KB transcript past the receiver's 8 KB cap.
    while ($body.Length -gt 7600 -and $report.log_tail.Length -gt 0) {
        $report.log_tail = $report.log_tail.Substring([int][Math]::Ceiling($report.log_tail.Length / 2))
        $body = $report | ConvertTo-Json -Compress
    }
    try { Invoke-RestMethod -Method Post -Uri "$Web/api/attemptdb/install-report" -ContentType "application/json; charset=utf-8" -Body ([System.Text.Encoding]::UTF8.GetBytes($body)) -TimeoutSec 5 | Out-Null } catch {}
    Stop-InstallTranscript
}
function Fail { param([string]$Message) $script:LastError = $Message; Write-Fingerprint; Send-Report $false; Write-Error "vibemon: $Message"; exit 1 }

# Download/extraction/filesystem exceptions can bypass every explicit Fail
# call. A nonzero process exit must still report the stage exactly once.
trap {
    if (-not $script:LastError) { $script:LastError = $_.Exception.Message }
    if (-not $script:Reported) { Write-Fingerprint }
    Send-Report $false
    Write-Host ("vibemon: " + $script:LastError)
    exit 1
}

# An explicit capture mode (-MetadataOnly, -LocalContent, or the environment):
# the only thing that changes an existing database's mode.
$ExplicitMode = ""
if ($env:VIBEMON_CAPTURE_MODE) { $ExplicitMode = $env:VIBEMON_CAPTURE_MODE }
if ($MetadataOnly) { $ExplicitMode = "metadata_only" }
if ($LocalContent) { $ExplicitMode = "local_semantic" }
if ($ExplicitMode -ne "" -and (@("metadata_only", "local_semantic", "full_sync") -notcontains $ExplicitMode)) {
    Fail "unknown capture mode (metadata_only | local_semantic | full_sync); nothing changed"
}

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
    if (-not $ApiKey.StartsWith("vbm_")) { Fail "invalid VibeMon API key; use the VibeMon account key (vbm_...) or copy a new installation command from https://vibemon.dev/devices (nothing paired)" }
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
    if (-not $Pair.StartsWith("pair_")) { Fail "invalid pairing token; copy a new installation command from https://vibemon.dev/devices (nothing paired)" }
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
# 2. The binary (install.ps1 verifies SHA256SUMS). ATTEMPTDB_NO_SETUP keeps it
#    to the binary: from 0.2.14 install.ps1 also runs `attempt setup`, which
#    would wire hooks and the daemon before this script has paired. Skipped
#    when the machine already has the pinned version or newer.
$present = $null
$cmd = Get-Command attempt -ErrorAction SilentlyContinue
if ($cmd) {
    $out = (& attempt --version 2>$null)
    if ($out -match '(\d+\.\d+\.\d+)') { try { $present = [version]$Matches[1] } catch { $present = $null } }
}
if ($present -and $present -ge [version]$AttemptVersion) {
    Write-Host "attempt $present present (need $AttemptVersion or newer); keeping it"
} elseif ($DryRun) {
    Write-Host "+ `$env:ATTEMPTDB_VERSION=$AttemptVersion; `$env:ATTEMPTDB_NO_SETUP=1; irm $Installer | iex"
} else {
    if ($present) { Write-Host "attempt $present present; installing $AttemptVersion" }
    $previousNoSetup = $env:ATTEMPTDB_NO_SETUP
    $previousModifyPath = $env:ATTEMPTDB_MODIFY_PATH
    $env:ATTEMPTDB_NO_SETUP = "1"
    # Putting the install directory on the user PATH is what this script has
    # always done, and it must not turn into a question in the middle of a
    # pairing; install.ps1 only edits it without asking when told to.
    if (-not $previousModifyPath) { $env:ATTEMPTDB_MODIFY_PATH = "1" }
    try { Invoke-Expression (Invoke-RestMethod $Installer) }
    finally { $env:ATTEMPTDB_NO_SETUP = $previousNoSetup; $env:ATTEMPTDB_MODIFY_PATH = $previousModifyPath }
    if (-not (Get-Command attempt -ErrorAction SilentlyContinue)) { Fail "attempt is not on PATH after install; add $BinDir to PATH and re-run" }
}

$Step = "init"
# 3. The local database. A new one is created local_semantic; an existing one
#    is left exactly as it is (mode, settings, data) unless a mode was asked
#    for: RFC 0006 section 2 forbids an installer from raising a metadata_only
#    database on its own, because that is the user's consent to give. The mode
#    in effect is printed.
$exists = $false
if (-not $DryRun) { try { attempt status *> $null; $exists = ($LASTEXITCODE -eq 0) } catch { $exists = $false } }
if ($exists) {
    $existingMode = ""
    try { $existingMode = ((attempt status --json 2>$null | ConvertFrom-Json).capture_mode) } catch { $existingMode = "" }
    if (-not $existingMode) { $existingMode = "unknown" }
    if ($ExplicitMode -ne "") {
        if (-not (Invoke-Step @("attempt", "init", "--capture-mode", $ExplicitMode, "--source", "vibemon"))) { Fail "attempt init failed" }
        $modeInEffect = $ExplicitMode
        $modeNote = "set by you, was $existingMode"
    } else {
        if (-not (Invoke-Step @("attempt", "init", "--source", "vibemon"))) { Fail "attempt init failed" }
        $modeInEffect = $existingMode
        $modeNote = "existing database, kept as it was"
    }
} else {
    $modeInEffect = if ($ExplicitMode -ne "") { $ExplicitMode } else { "local_semantic" }
    $modeNote = if ($ExplicitMode -ne "") { "new database, set by you" } else { "new database" }
    if (-not (Invoke-Step @("attempt", "init", "--capture-mode", $modeInEffect, "--source", "vibemon"))) { Fail "attempt init failed" }
}
Write-Host "vibemon: capture mode: $modeInEffect ($modeNote)"
if ($modeInEffect -eq "metadata_only" -and ($Profile -eq "messages" -or $Profile -eq "full")) {
    Write-Host "vibemon: this machine stores no conversation text, so the '$Profile' profile has none to upload."
    Write-Host "         To keep and upload it, re-run with -LocalContent (it stays encrypted on this machine)."
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
