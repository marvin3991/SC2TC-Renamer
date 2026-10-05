chcp 65001 > $null
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot
$taskPythonCandidates = @()
$taskPythonCommand = Get-Command python -ErrorAction SilentlyContinue
if ($null -ne $taskPythonCommand) {
    $taskPythonCandidates += @{ Executable = $taskPythonCommand.Source; Prefix = @() }
}
$taskPyLauncher = Get-Command py -ErrorAction SilentlyContinue
if ($null -ne $taskPyLauncher) {
    $taskPythonCandidates += @{ Executable = $taskPyLauncher.Source; Prefix = @('-3') }
}
$taskPython = $null
foreach ($taskCandidate in $taskPythonCandidates) {
    $taskExecutable = $taskCandidate.Executable
    $taskPrefix = $taskCandidate.Prefix
    try {
        & $taskExecutable @taskPrefix -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' *> $null
        if ($LASTEXITCODE -eq 0) {
            $taskPython = $taskCandidate
            break
        }
    } catch {
        # Try the other installed interpreter; no installation or global setting change.
    }
}
if ($null -eq $taskPython) { throw '打包需要可執行的 Python 3.11 以上版本（python 或 py -3）；執行檔本身不依賴 Python。' }
$taskExecutable = $taskPython.Executable
$taskPrefix = $taskPython.Prefix
& $taskExecutable @taskPrefix (Join-Path $PSScriptRoot 'scripts\package-rust.py')
if ($LASTEXITCODE -ne 0) { throw 'Rust 建置或對應原始碼打包失敗；請保留 dist 中的診斷紀錄。' }
