# AttemptDB installer for Windows.
#
#   irm https://raw.githubusercontent.com/nullarch/attemptdb/main/install.ps1 | iex
#
# Downloads a release archive, verifies its checksum, installs `attempt.exe`,
# and then runs `attempt setup`: the local database, hook entries in every
# coding agent found on this machine (next to whatever is already there),
# the background daemon, and a check. Run it again any time: it
# upgrades the binary and repairs the wiring. `attempt uninstall` reverses it.
#
# Environment:
#   ATTEMPTDB_VERSION   version to install (default: latest release)
#   ATTEMPTDB_BIN_DIR   install directory (default: %LOCALAPPDATA%\AttemptDB\bin)
#   ATTEMPTDB_NO_SETUP  set to 1 to install the binary only and touch no
#                       coding agent's configuration (`attempt setup` later)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'nullarch/attemptdb'
$BinDir = if ($env:ATTEMPTDB_BIN_DIR) { $env:ATTEMPTDB_BIN_DIR }
          else { Join-Path $env:LOCALAPPDATA 'AttemptDB\bin' }

# TLS 1.2 for Windows PowerShell 5.1, which still defaults lower.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# ---- target detection ------------------------------------------------------

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "unsupported architecture: $($env:PROCESSOR_ARCHITECTURE)" }
}
$target = "$arch-pc-windows-msvc"

# ---- version resolution ----------------------------------------------------

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

# ---- download and verify ---------------------------------------------------

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

    # Verification is not optional. This script is run as `irm … | iex`, so a
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

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $exe = Join-Path $tmp "$stem\attempt.exe"
    if (-not (Test-Path $exe)) { throw 'archive did not contain attempt.exe' }

    # ---- install -----------------------------------------------------------

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

    # ---- PATH --------------------------------------------------------------

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    $onPath = $userPath -split ';' | Where-Object { $_ -eq $BinDir }
    if (-not $onPath) {
        $newPath = if ($userPath.TrimEnd(';')) { $userPath.TrimEnd(';') + ';' + $BinDir } else { $BinDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = $env:Path + ';' + $BinDir
        Write-Host "Added $BinDir to your user PATH (open a new terminal for it to apply)."
    }

    if ($env:ATTEMPTDB_NO_SETUP -eq '1') {
        Write-Host ''
        Write-Host 'Next (ATTEMPTDB_NO_SETUP=1 skipped this):'
        Write-Host '  attempt setup         # database, agent hooks, background daemon, check'
        Write-Host ''
        Write-Host 'Nothing is uploaded anywhere. There is no account and no telemetry.'
        return
    }

    # ---- setup -------------------------------------------------------------
    # The binary knows how to wire a machine; this script only downloads it.
    # `attempt setup` is idempotent and reports every step. Releases before
    # 0.2.14 have none (ATTEMPTDB_VERSION can ask for one, and this script on
    # `main` can briefly run ahead of the newest release).
    & $dest setup --help *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host ''
        Write-Host "attempt $version predates 'attempt setup'. Wire this machine with:"
        Write-Host '  attempt init; attempt hook install; attempt daemon install'
        Write-Host ''
        Write-Host 'Nothing is uploaded anywhere. There is no account and no telemetry.'
        return
    }
    Write-Host ''
    & $dest setup --source install.ps1
    $setupExit = $LASTEXITCODE
    Write-Host ''
    Write-Host 'Nothing is uploaded anywhere. There is no account and no telemetry.'
    if ($setupExit -ne 0) {
        Write-Host "attempt setup finished with problems (exit $setupExit); fix them and run 'attempt setup' again."
    }
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
