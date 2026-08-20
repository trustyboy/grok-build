[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments)]
    [string[]] $CargoArguments
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 定位可打包进二进制的 ripgrep。
#
# Windows 上 build.rs 不会自动下载 ripgrep（官方只发 .zip，构建脚本没有解压路径），
# 只有显式提供 GROK_TOOLS_BUNDLE_RG_PATH 才会打包。不打包时 grep 工具退化为依赖
# 目标机器 PATH 中的 rg，缺失时只报 "program not found"，排查成本很高。
#
# 因此这里自动发现本机 ripgrep 并打包，让产物自包含：构建一次，exe 复制到任何
# Windows 机器都能用 grep，无需逐台安装 ripgrep。
function Resolve-RipgrepPath {
    $command = Get-Command 'rg' -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $command) { return $null }

    $item = Get-Item -LiteralPath $command.Source
    # winget 安装的 rg 是「命令别名」符号链接（长度为 0），必须解析到真实可执行文件，
    # 否则打包进去的会是一个空文件。
    if ($item.LinkType) {
        $target = $item.ResolveLinkTarget($true)
        if ($target) { return $target.FullName }
    }
    return $item.FullName
}

$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$SetupScript = Join-Path $PSScriptRoot 'setup.ps1'
$PreviousProtoc = [Environment]::GetEnvironmentVariable('PROTOC', 'Process')
$PreviousBundleRg = [Environment]::GetEnvironmentVariable('GROK_TOOLS_BUNDLE_RG_PATH', 'Process')
$WrapperPath = (& $SetupScript | Select-Object -Last 1)

if (-not (Test-Path -LiteralPath $WrapperPath -PathType Leaf)) {
    throw "初始化脚本没有生成 protoc Wrapper：$WrapperPath"
}

try {
    [Environment]::SetEnvironmentVariable('PROTOC', $WrapperPath, 'Process')

    # 找不到 ripgrep 不阻断构建，只是产物会退回依赖目标机器 PATH。
    $RipgrepPath = Resolve-RipgrepPath
    if ($RipgrepPath) {
        [Environment]::SetEnvironmentVariable('GROK_TOOLS_BUNDLE_RG_PATH', $RipgrepPath, 'Process')
        Write-Host "将打包 ripgrep：$RipgrepPath"
    }
    else {
        Write-Warning '未找到 ripgrep，产物的 grep 工具将依赖目标机器 PATH 中的 rg。'
        Write-Warning '建议先安装后再构建：winget install BurntSushi.ripgrep.MSVC'
    }

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
    [Environment]::SetEnvironmentVariable('GROK_TOOLS_BUNDLE_RG_PATH', $PreviousBundleRg, 'Process')
}

