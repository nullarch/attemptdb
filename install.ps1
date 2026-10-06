# AttemptDB installer for Windows.
#
#   irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1 | iex
#
# Downloads a release archive, verifies its checksum, and installs
# `attempt.exe` and `attempt-hook.exe`. Then it shows what `attempt setup` would
# change - the local database, hook entries in every coding agent found on this
# machine (next to whatever is already there), the OpenTelemetry exporter
# settings of Claude Code and Codex, and the background task - and asks before
# it touches any of that:
#
#   a console you can type in   shows the preview, asks "Apply these changes? [Y/n]"
#   no console (CI, an agent)   installs the binary only, changes nothing else,
#                               and prints the exact command that would
#   -Yes / ATTEMPTDB_ASSUME_YES=1   applies without asking
#
# Run it again any time: it upgrades the binary and, with consent, repairs the
# wiring. `attempt uninstall` reverses it.
#
# The whole script is wrapped in functions and runs from its last line, so a
# download that is cut off part-way executes nothing.
#
# `irm | iex` cannot take parameters; to pass -Yes:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1))) -Yes
# or set the environment variable first:
#   $env:ATTEMPTDB_ASSUME_YES = "1"; irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1 | iex
#
# Environment:
#   ATTEMPTDB_VERSION   version to install (default: latest release)
#   ATTEMPTDB_BIN_DIR   install directory (default: %LOCALAPPDATA%\AttemptDB\bin)
#   ATTEMPTDB_NO_SETUP=1
#                       install the binary only and touch no coding agent's
#                       configuration (`attempt setup` later)
#   ATTEMPTDB_ASSUME_YES=1
#                       same as -Yes
#   ATTEMPTDB_MODIFY_PATH=1|0
#                       add the install directory to your user PATH (1) or
#                       leave PATH alone (0), without asking. Unset: ask when
#                       there is a console, otherwise only print the hint
#   ATTEMPTDB_NO_MODIFY_PATH=1
#                       never touch PATH (wins over the above)
#   ATTEMPTDB_VERIFY_ATTESTATION=1
#                       also verify the archive's build provenance with
#                       `gh attestation verify` (needs the GitHub CLI); a failed
#                       or impossible verification stops the install
#   ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1
#                       install without verifying the download. Do not.
#
# Verify a download by hand, with the archive and SHA256SUMS from the release:
#   (Get-FileHash attempt-<version>-<target>.zip -Algorithm SHA256).Hash
#   gh attestation verify attempt-<version>-<target>.zip --repo nullarch/attemptdb

param([switch]$Yes)

# ---- helpers ---------------------------------------------------------------

# A person can be asked: a user session whose input is not redirected. Anything
# that goes wrong finding out (ISE, a service host) means no.
function Test-AttemptDbInteractive {
    try {
        return ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected)
    } catch {
        return $false
    }
}

# Ask a yes/no question. Enter takes the default; anything unreadable is a no.
function Read-AttemptDbAnswer {
    param([string]$Question, [bool]$DefaultYes)
    $hint = if ($DefaultYes) { '[Y/n]' } else { '[y/N]' }
    $reply = $null
    try { $reply = Read-Host ($Question + ' ' + $hint) } catch { return $false }
    if ($null -eq $reply) { return $false }
    $reply = ([string]$reply).Trim()
    if ($reply -eq '') { return $DefaultYes }
    return [bool]($reply -match '^(y|yes)$')
}

# The user PATH exactly as the registry stores it: %VARIABLES% unexpanded, and
# whether it is REG_SZ or REG_EXPAND_SZ. [Environment]::GetEnvironmentVariable
# expands them, and writing that back (as this script once did) turns every
# %USERPROFILE%\... entry into a fixed string.
function Get-AttemptDbUserPath {
    $value = ''
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    if ($null -ne $key) {
        try {
            $raw = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if ($null -ne $raw) {
                $value = [string]$raw
                $kind = $key.GetValueKind('Path')
            }
        } finally {
            $key.Close()
        }
    }
    return @{ Value = $value; Kind = $kind }
}

# Is Directory one of the entries of a PATH value? Case-insensitive, trailing
# backslash ignored, and an entry such as %LOCALAPPDATA%\x counts as what it
# expands to.
function Test-AttemptDbPathContains {
    param([string]$PathValue, [string]$Directory)
    if (-not $PathValue) { return $false }
    $want = $Directory.TrimEnd('\').ToLowerInvariant()
    foreach ($entry in ($PathValue -split ';')) {
        if ($entry.Trim() -eq '') { continue }
        $candidates = @($entry, [Environment]::ExpandEnvironmentVariables($entry))
        foreach ($candidate in $candidates) {
            if ($candidate.Trim().TrimEnd('\').ToLowerInvariant() -eq $want) { return $true }
        }
    }
    return $false
}

# The PATH value with Directory appended, or $null when it is already there.
function Join-AttemptDbPathEntry {
    param([string]$PathValue, [string]$Directory)
    if (Test-AttemptDbPathContains $PathValue $Directory) { return $null }
    $trimmed = if ($PathValue) { $PathValue.TrimEnd(';') } else { '' }
    if ($trimmed) { return ($trimmed + ';' + $Directory) }
    return $Directory
}

# Append Directory to the user PATH, keeping every other entry byte for byte
# and the value's registry type. Returns $false when nothing needed changing.
function Add-AttemptDbUserPath {
    param([string]$Directory)
    $current = Get-AttemptDbUserPath
    $new = Join-AttemptDbPathEntry $current.Value $Directory
    if ($null -eq $new) { return $false }
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $key.SetValue('Path', $new, $current.Kind)
    } finally {
        $key.Close()
    }
    # Writing the registry does not tell running programs. .NET broadcasts the
    # change whenever it writes a user variable, so write and remove a marker.
    try {
        [Environment]::SetEnvironmentVariable('ATTEMPTDB_PATH_REFRESH', '1', 'User')
        [Environment]::SetEnvironmentVariable('ATTEMPTDB_PATH_REFRESH', $null, 'User')
    } catch {
        # Not worth failing over: a new terminal after sign-in sees the value.
    }
    return $true
}

# The installed binary the way a person can type it right now.
function Get-AttemptDbCommand {
    param([string]$BinDir)
    if (Test-AttemptDbPathContains $env:Path $BinDir) { return 'attempt' }
    return ('& "' + (Join-Path $BinDir 'attempt.exe') + '"')
}

# ---- setup -----------------------------------------------------------------
#
# The binary knows how to wire a machine; this script only downloads it. It
# also does not wire one without consent: `attempt setup` edits other tools'
# configuration files, so it is shown (--dry-run) and then applied only when the
# person says yes at a console, or asked for it with -Yes /
# ATTEMPTDB_ASSUME_YES=1. `attempt setup` is idempotent and reports every step.
# Releases before 0.2.14 have none (ATTEMPTDB_VERSION can ask for one, and this
# script on `main` can briefly run ahead of the newest release).

function Invoke-AttemptDbSetup {
    param([string]$Exe, [string]$Version, [string]$BinDir, [bool]$AssumeYes, [bool]$Interactive)

    $cmd = Get-AttemptDbCommand $BinDir
    if ($env:ATTEMPTDB_NO_SETUP -eq '1') {
        Write-Host ''
        Write-Host 'Next (ATTEMPTDB_NO_SETUP=1 skipped this):'
        Write-Host ("  $cmd setup --dry-run   # what it would change; writes nothing")
        Write-Host ("  $cmd setup             # database, agent hooks, background task, check")
        return
    }

    $hasSetup = $false
    try {
        & $Exe setup --help *> $null
        $hasSetup = ($LASTEXITCODE -eq 0)
    } catch {
        $hasSetup = $false
    }
    if (-not $hasSetup) {
        Write-Host ''
        Write-Host "attempt $Version predates 'attempt setup'. Wire this machine with:"
        Write-Host "  $cmd init; $cmd hook install; $cmd daemon install"
        return
    }

    if ($AssumeYes -or ($env:ATTEMPTDB_ASSUME_YES -eq '1')) {
        Write-Host ''
        Invoke-AttemptDbApply $Exe
        return
    }

    Write-Host ''
    Write-Host 'This is what setup would change on this machine (nothing is written yet):'
    Write-Host ''
    & $Exe setup --dry-run --source install.ps1
    # 1 is a report with problems in it, which the person should still see and
    # judge; anything above is `attempt` refusing its arguments.
    if ($LASTEXITCODE -gt 1) {
        Write-Host ("attempt setup --dry-run failed (exit " + $LASTEXITCODE + "); nothing was changed.")
        return
    }

    if ($Interactive) {
        Write-Host ''
        if (Read-AttemptDbAnswer 'Apply these changes?' $true) {
            Write-Host ''
            Invoke-AttemptDbApply $Exe
        } else {
            Write-Host ''
            Write-Host 'Nothing was changed. The attempt binary is installed; apply the changes later with:'
            Write-Host "  $cmd setup"
        }
        return
    }

    Write-Host ''
    Write-Host 'There is no console to ask on, so nothing was changed beyond installing the'
    Write-Host 'binary. To apply the changes above:'
    Write-Host "  $cmd setup"
    Write-Host 'or run this installer again, telling it to go ahead:'
    Write-Host '  $env:ATTEMPTDB_ASSUME_YES = "1"; irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1 | iex'
}

function Invoke-AttemptDbApply {
    param([string]$Exe)
    & $Exe setup --source install.ps1
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        Write-Host ''
        Write-Host ("attempt setup finished with problems (exit " + $exitCode + "); fix them and run 'attempt setup' again.")
    }
}

# ---- PATH ------------------------------------------------------------------
#
# Hooks and the background task use the binary's absolute path, so capture works
# with the directory off PATH; PATH only matters for typing `attempt`. Adding it
# edits the user's environment, which is done with consent only: asked at a
# console (default yes), or ATTEMPTDB_MODIFY_PATH=1.

function Invoke-AttemptDbPathStep {
    param([string]$BinDir, [bool]$Interactive)

    $current = Get-AttemptDbUserPath
    if ((Test-AttemptDbPathContains $current.Value $BinDir) -or (Test-AttemptDbPathContains $env:Path $BinDir)) {
        return
    }

    $consent = $false
    $declined = ($env:ATTEMPTDB_NO_MODIFY_PATH -eq '1') -or ($env:ATTEMPTDB_MODIFY_PATH -eq '0')
    if (-not $declined) {
        if ($env:ATTEMPTDB_MODIFY_PATH -eq '1') {
            $consent = $true
        } elseif ($Interactive) {
            Write-Host ''
            Write-Host "$BinDir is not on your PATH."
            $consent = Read-AttemptDbAnswer 'Add it to your user PATH?' $true
        }
    }

    if ($consent) {
        try {
            [void](Add-AttemptDbUserPath $BinDir)
            $env:Path = $env:Path.TrimEnd(';') + ';' + $BinDir
            Write-Host ''
            Write-Host "Added $BinDir to your user PATH (open a new terminal for it to apply)."
            return
        } catch {
            Write-Host ''
            Write-Host ('Could not change your user PATH: ' + $_.Exception.Message)
        }
    }

    Write-Host ''
    Write-Host "$BinDir is not on your PATH, so type this for now:"
    Write-Host ('  & "' + (Join-Path $BinDir 'attempt.exe') + '"')
    Write-Host 'To add it, run this installer again with $env:ATTEMPTDB_MODIFY_PATH = "1", or edit'
    Write-Host 'your user PATH in Settings > System > About > Advanced system settings > Environment Variables.'
}

# ---- install ---------------------------------------------------------------

function Install-AttemptDb {
    param([bool]$AssumeYes)

    $ErrorActionPreference = 'Stop'
    Set-StrictMode -Version Latest

    $Repo = 'nullarch/attemptdb'
    $BinDir = if ($env:ATTEMPTDB_BIN_DIR) { $env:ATTEMPTDB_BIN_DIR }
              else { Join-Path $env:LOCALAPPDATA 'AttemptDB\bin' }

    # TLS 1.2 for Windows PowerShell 5.1, which still defaults lower. Added to
    # the protocols already enabled, not substituted for them.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # ---- target detection ----------------------------------------------------

    $arch = switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'x86_64' }
        'ARM64' { 'aarch64' }
        default { throw "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
    }
    $target = "$arch-pc-windows-msvc"

    # ---- version resolution --------------------------------------------------

    $version = $env:ATTEMPTDB_VERSION
    if (-not $version) {
        Write-Host 'Resolving the latest release...'
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
                                         -Headers @{ 'User-Agent' = 'attemptdb-installer' }
            $version = $release.tag_name
        } catch {
            throw "could not resolve the latest release. Is one published yet?`n" +
                  "Build from source instead:`n" +
                  "  git clone https://github.com/$Repo`n" +
                  "  cd attemptdb; cargo install --path crates/attempt"
        }
    }
    $version = $version -replace '^v', ''

    $stem = "attempt-$version-$target"
    $base = "https://github.com/$Repo/releases/download/v$version"

    # ---- download and verify -------------------------------------------------

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("attemptdb-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null

    try {
        $zip = Join-Path $tmp "$stem.zip"
        Write-Host "Downloading $stem..."
        try {
            Invoke-WebRequest -Uri "$base/$stem.zip" -OutFile $zip -UseBasicParsing
        } catch {
            throw "no release asset for $target in v$version"
        }

        # Verification is not optional. This script is run as `irm ... | iex`, so a
        # missing checksum file must stop the install rather than downgrade it to an
        # unverified one: whoever can remove SHA256SUMS from a release can replace
        # the zip beside it. Every release publishes SHA256SUMS.
        $skip = $env:ATTEMPTDB_INSECURE_SKIP_CHECKSUM -eq '1'
        $sums = Join-Path $tmp 'SHA256SUMS'
        if ($skip) {
            Write-Warning 'ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 - installing WITHOUT verifying the download'
        } else {
            try {
                Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing
            } catch {
                throw "SHA256SUMS is not published for v$version, so this download cannot be verified. Refusing to install. Set ATTEMPTDB_INSECURE_SKIP_CHECKSUM=1 to override."
            }
        }

        if (-not $skip) {
            $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
            $line = Select-String -Path $sums -Pattern ([Regex]::Escape("$stem.zip") + '$') |
                    Select-Object -First 1
            if (-not $line) { throw "$stem.zip is not listed in SHA256SUMS" }
            $expected = ($line.Line -split '\s+')[0].ToLower()
            if ($actual -ne $expected) {
                throw "checksum mismatch`n  expected $expected`n  actual   $actual"
            }
            Write-Host 'Checksum verified.'
        }

        # Optional, stronger: who built it. SHA256SUMS only proves the archive is
        # the one the release published; the attestation proves this repository's
        # workflow produced it. Asked for explicitly, so asking and not getting it
        # is a failure, never a silent downgrade.
        if ($env:ATTEMPTDB_VERIFY_ATTESTATION -eq '1') {
            if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
                throw "ATTEMPTDB_VERIFY_ATTESTATION=1 needs the GitHub CLI (gh), which was not found. Install it (https://cli.github.com), or unset the variable to rely on the SHA-256 check alone."
            }
            Write-Host 'Verifying build provenance...'
            & gh attestation verify $zip --repo $Repo
            if ($LASTEXITCODE -ne 0) {
                throw "build provenance verification failed for $stem.zip; refusing to install. (gh needs a login or GH_TOKEN. To see why, run it by hand: gh attestation verify $stem.zip --repo $Repo)"
            }
            Write-Host 'Build provenance verified.'
        }

        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        $exe = Join-Path $tmp "$stem\attempt.exe"
        if (-not (Test-Path $exe)) { throw 'archive did not contain attempt.exe' }

        # ---- install ---------------------------------------------------------

        New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
        $dest = Join-Path $BinDir 'attempt.exe'

        # Windows refuses to overwrite a running executable; move it aside first.
        if (Test-Path $dest) {
            $old = Join-Path $BinDir ('attempt.exe.old-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
            try { Move-Item -Path $dest -Destination $old -Force } catch {
                throw "attempt.exe is in use. Stop it first: attempt daemon stop"
            }
            Remove-Item -Path $old -Force -ErrorAction SilentlyContinue
        }
        Copy-Item -Path $exe -Destination $dest -Force

        # The dedicated hook executable (releases from 0.2.0): `attempt hook
        # install` references it when it sits next to attempt.exe.
        $hookExe = Join-Path $tmp "$stem\attempt-hook.exe"
        if (Test-Path $hookExe) {
            $hookDest = Join-Path $BinDir 'attempt-hook.exe'
            if (Test-Path $hookDest) {
                $oldHook = Join-Path $BinDir ('attempt-hook.exe.old-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
                try { Move-Item -Path $hookDest -Destination $oldHook -Force } catch {
                    throw "attempt-hook.exe is in use; retry in a moment"
                }
                Remove-Item -Path $oldHook -Force -ErrorAction SilentlyContinue
            }
            Copy-Item -Path $hookExe -Destination $hookDest -Force
        }

        Write-Host ''
        Write-Host "Installed attempt $version to $dest"

        $interactive = Test-AttemptDbInteractive
        Invoke-AttemptDbSetup -Exe $dest -Version $version -BinDir $BinDir -AssumeYes $AssumeYes -Interactive $interactive
        Invoke-AttemptDbPathStep -BinDir $BinDir -Interactive $interactive
        Write-Host ''
        Write-Host 'Nothing is uploaded anywhere. There is no account and no telemetry.'
    } finally {
        Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Install-AttemptDb -AssumeYes ($Yes.IsPresent)
