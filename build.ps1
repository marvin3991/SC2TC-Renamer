chcp 65001 > $null
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'build-rust.ps1')
