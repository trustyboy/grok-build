[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments)]
    [string[]] $CargoArguments = @()
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 正式构建入口与测试入口共享工具环境，保持编译缓存可复用。
& (Join-Path $PSScriptRoot 'invoke-cargo-release.ps1') -CargoCommand build `
    -CargoArguments (@('-p', 'xai-grok-pager-bin') + @($CargoArguments))
