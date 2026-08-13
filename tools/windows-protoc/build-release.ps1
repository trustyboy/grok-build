[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments)]
    [string[]] $CargoArguments
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$SetupScript = Join-Path $PSScriptRoot 'setup.ps1'
$PreviousProtoc = [Environment]::GetEnvironmentVariable('PROTOC', 'Process')
$WrapperPath = (& $SetupScript | Select-Object -Last 1)

if (-not (Test-Path -LiteralPath $WrapperPath -PathType Leaf)) {
    throw "初始化脚本没有生成 protoc Wrapper：$WrapperPath"
}

try {
    [Environment]::SetEnvironmentVariable('PROTOC', $WrapperPath, 'Process')
    Push-Location -LiteralPath $RepositoryRoot
    try {
        & cargo build -p xai-grok-pager-bin --release @CargoArguments
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo Release 构建失败，退出码：$LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    [Environment]::SetEnvironmentVariable('PROTOC', $PreviousProtoc, 'Process')
}

