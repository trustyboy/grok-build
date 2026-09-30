[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('build', 'test', 'check')]
    [string] $CargoCommand,

    [string[]] $CargoArguments = @()
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 保持构建和测试使用相同的 ripgrep 实体路径，避免环境变化触发重复编译。
function Resolve-RipgrepPath {
    $command = Get-Command 'rg' -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $command) { return $null }

    $item = Get-Item -LiteralPath $command.Source
    if ($item.LinkType) {
        $target = $item.ResolveLinkTarget($true)
        if ($target) { return $target.FullName }
    }
    return $item.FullName
}

# 测试程序的参数位于 -- 之后；这里只校验传给 Cargo 的配置。
foreach ($argument in $CargoArguments) {
    if ($argument -eq '--') { break }
    if ($argument -eq '--profile' -or $argument.StartsWith('--profile=')) {
        throw '此入口固定使用 --release，请勿另设 --profile。'
    }
}

$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$PreviousProtoc = [Environment]::GetEnvironmentVariable('PROTOC', 'Process')
$PreviousBundleRg = [Environment]::GetEnvironmentVariable('GROK_TOOLS_BUNDLE_RG_PATH', 'Process')
$WrapperPath = (& (Join-Path $PSScriptRoot 'setup.ps1') | Select-Object -Last 1)
if (-not (Test-Path -LiteralPath $WrapperPath -PathType Leaf)) {
    throw "初始化脚本没有生成 protoc Wrapper：$WrapperPath"
}

try {
    [Environment]::SetEnvironmentVariable('PROTOC', $WrapperPath, 'Process')
    $RipgrepPath = Resolve-RipgrepPath
    if ($RipgrepPath) {
        [Environment]::SetEnvironmentVariable('GROK_TOOLS_BUNDLE_RG_PATH', $RipgrepPath, 'Process')
        Write-Host "将打包 ripgrep：$RipgrepPath"
    }
    else {
        Write-Warning '未找到 ripgrep，未指定打包路径时产物的 grep 工具将依赖目标机器 PATH 中的 rg。'
        Write-Warning '建议先安装后再构建：winget install BurntSushi.ripgrep.MSVC'
    }

    Push-Location -LiteralPath $RepositoryRoot
    try {
        & cargo $CargoCommand --release @CargoArguments
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo Release $CargoCommand 失败，退出码：$LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    [Environment]::SetEnvironmentVariable('PROTOC', $PreviousProtoc, 'Process')
    [Environment]::SetEnvironmentVariable('GROK_TOOLS_BUNDLE_RG_PATH', $PreviousBundleRg, 'Process')
}
