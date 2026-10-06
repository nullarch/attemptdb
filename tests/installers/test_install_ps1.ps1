# install.ps1 checks that need no network and change nothing on the machine.
# Run on Windows CI:  powershell -NoProfile -File tests/installers/test_install_ps1.ps1
#
# 1. The file parses, and nothing runs before its last line: every top-level
#    statement is a function definition except the final call, so a download
#    that is cut off part-way executes nothing.
# 2. The user-PATH helpers keep every existing entry byte for byte (including
#    %VARIABLES%) and never add a directory twice.
# 3. `attempt setup` is never applied without consent: with no console and no
#    -Yes only the read-only preview runs; -Yes applies once; NO_SETUP runs
#    nothing. The fake `attempt` is a .cmd file that records its arguments.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$path = Join-Path $PSScriptRoot '../../install.ps1'
$tokens = $null; $parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path $path), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -gt 0) { throw ($parseErrors | Out-String) }

function Assert-Equal {
    param($Actual, $Expected, [string]$What)
    if ($Actual -ne $Expected) { throw ("$What" + ": expected '$Expected', got '$Actual'") }
}

# ---- 1. nothing runs before the last line --------------------------------------

$statements = @($ast.EndBlock.Statements)
if ($statements.Count -lt 2) { throw 'install.ps1 should be functions followed by one call' }
$last = $statements[$statements.Count - 1]
if ($last -isnot [System.Management.Automation.Language.PipelineAst]) { throw 'the last statement must be the call to Install-AttemptDb' }
Assert-Equal $last.PipelineElements[0].GetCommandName() 'Install-AttemptDb' 'the last statement'
for ($i = 0; $i -lt $statements.Count - 1; $i++) {
    if ($statements[$i] -isnot [System.Management.Automation.Language.FunctionDefinitionAst]) {
        throw ('a top-level statement runs before the final call: ' + $statements[$i].Extent.Text)
    }
}
if ($null -eq $ast.ParamBlock) { throw 'install.ps1 should declare -Yes in a param block' }
Write-Host 'PASS: install.ps1 parses and runs nothing before its last line'

# ---- load the helpers (never the installer itself) -----------------------------

foreach ($name in @('Test-AttemptDbPathContains', 'Join-AttemptDbPathEntry', 'Get-AttemptDbUserPath',
                    'Get-AttemptDbCommand', 'Read-AttemptDbAnswer', 'Invoke-AttemptDbApply', 'Invoke-AttemptDbSetup')) {
    $wanted = $name
    $fn = $ast.Find({ param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $wanted
    }, $true)
    if (-not $fn) { throw "install.ps1 has no function $name" }
    Invoke-Expression $fn.Extent.Text
}

# ---- 2. PATH entries -----------------------------------------------------------

Assert-Equal (Join-AttemptDbPathEntry '' 'C:\x') 'C:\x' 'empty PATH'
Assert-Equal (Join-AttemptDbPathEntry 'C:\a;' 'C:\x') 'C:\a;C:\x' 'trailing semicolon'
Assert-Equal (Join-AttemptDbPathEntry 'C:\a;C:\x' 'C:\x') $null 'already present'
Assert-Equal (Join-AttemptDbPathEntry 'C:\a;C:\X\' 'c:\x') $null 'case and trailing backslash'
# Existing entries come back untouched, %VARIABLES% included.
Assert-Equal (Join-AttemptDbPathEntry '%USERPROFILE%\bin;C:\a' 'C:\x') '%USERPROFILE%\bin;C:\a;C:\x' 'unexpanded entries survive'
if ($env:SystemRoot) {
    $expanded = [Environment]::ExpandEnvironmentVariables('%SystemRoot%\system32')
    Assert-Equal (Test-AttemptDbPathContains '%SystemRoot%\system32;C:\a' $expanded) $true 'an unexpanded entry counts as what it expands to'
    Assert-Equal (Join-AttemptDbPathEntry '%SystemRoot%\system32' $expanded) $null 'no duplicate of an expanded entry'
    # Read-only: the stored user PATH and its registry type, never written here.
    $stored = Get-AttemptDbUserPath
    if ($stored.Value -isnot [string]) { throw 'the stored user PATH should be a string' }
    if ($stored.Kind -isnot [Microsoft.Win32.RegistryValueKind]) { throw 'the stored user PATH should carry its registry type' }
}
Write-Host 'PASS: PATH helpers keep existing entries and never duplicate'

# ---- 3. consent before `attempt setup` ------------------------------------------

$work = Join-Path ([IO.Path]::GetTempPath()) ('install-ps1-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null
try {
    $calls = Join-Path $work 'calls.txt'
    $fake = Join-Path $work 'attempt.cmd'
    Set-Content -Path $fake -Encoding ASCII -Value @(
        '@echo off',
        'echo %* >>"%ATTEMPT_TEST_CALLS%"',
        'exit /b 0'
    )
    $env:ATTEMPT_TEST_CALLS = $calls

    function Get-Calls {
        if (-not (Test-Path $calls)) { return @() }
        return @(Get-Content $calls | ForEach-Object { $_.Trim() })
    }
    function Reset-Run {
        Remove-Item $calls -ErrorAction SilentlyContinue
        Remove-Item Env:ATTEMPTDB_NO_SETUP -ErrorAction SilentlyContinue
        Remove-Item Env:ATTEMPTDB_ASSUME_YES -ErrorAction SilentlyContinue
    }

    # No console, no -Yes: the preview only.
    Reset-Run
    Invoke-AttemptDbSetup -Exe $fake -Version '9.9.9' -BinDir $work -AssumeYes $false -Interactive $false | Out-Null
    $got = (Get-Calls) -join '|'
    Assert-Equal $got 'setup --help|setup --dry-run --source install.ps1' 'no console and no -Yes runs only the preview'

    # -Yes: applied once, never previewed.
    Reset-Run
    Invoke-AttemptDbSetup -Exe $fake -Version '9.9.9' -BinDir $work -AssumeYes $true -Interactive $false | Out-Null
    Assert-Equal ((Get-Calls) -join '|') 'setup --help|setup --source install.ps1' '-Yes applies setup'

    # ATTEMPTDB_ASSUME_YES=1 is the same as -Yes.
    Reset-Run
    $env:ATTEMPTDB_ASSUME_YES = '1'
    Invoke-AttemptDbSetup -Exe $fake -Version '9.9.9' -BinDir $work -AssumeYes $false -Interactive $false | Out-Null
    Assert-Equal ((Get-Calls) -join '|') 'setup --help|setup --source install.ps1' 'ATTEMPTDB_ASSUME_YES=1 applies setup'

    # ATTEMPTDB_NO_SETUP=1 wins over -Yes: no `attempt` call at all.
    Reset-Run
    $env:ATTEMPTDB_NO_SETUP = '1'
    Invoke-AttemptDbSetup -Exe $fake -Version '9.9.9' -BinDir $work -AssumeYes $true -Interactive $false | Out-Null
    Assert-Equal ((Get-Calls) -join '|') '' 'ATTEMPTDB_NO_SETUP=1 runs nothing'
    Reset-Run
} finally {
    Remove-Item Env:ATTEMPT_TEST_CALLS -ErrorAction SilentlyContinue
    Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Host 'PASS: attempt setup is applied only with consent'
