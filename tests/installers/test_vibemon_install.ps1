$ErrorActionPreference = 'Stop'
$path = Join-Path $PSScriptRoot '../../docs/migration/vibemon-install.ps1'
$tokens = $null; $parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path $path), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -gt 0) { throw ($parseErrors | Out-String) }
# Load only the helper under test; never execute the actual installer.
$step = $ast.Find({ param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Invoke-Step'
}, $true)
Invoke-Expression $step.Extent.Text
$DryRun = $false
$engine = (Get-Process -Id $PID).Path
foreach ($exitCode in @(0, 7)) {
    $result = Invoke-Step @($engine, '-NoProfile', '-NonInteractive', '-Command', "Write-Output 'fixture output'; exit $exitCode")
    if ($result -isnot [bool]) { throw 'Command output contaminated the boolean result' }
    if ($result -ne ($exitCode -eq 0)) { throw "Wrong result for exit $exitCode" }
}
Write-Host 'PASS: PowerShell failure gates respect exit status even with native stdout'
