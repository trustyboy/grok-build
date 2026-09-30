# 直接转发原始参数，避免 Cargo 的 -p 与 PowerShell 公共参数缩写冲突。
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 沿用正式构建的 protoc 和 ripgrep 环境；包名和测试过滤条件直接传给 Cargo。
& (Join-Path $PSScriptRoot 'invoke-cargo-release.ps1') -CargoCommand test `
    -CargoArguments $args
