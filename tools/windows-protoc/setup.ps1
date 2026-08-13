[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProtocVersion = '29.3'
$ArchiveSha256 = '57EA59E9F551AD8D71FFAA9B5CFBE0CA1F4E720972A1DB7EC2D12AB44BFF9383'
$ArchiveName = "protoc-$ProtocVersion-win64.zip"
$DownloadUri = "https://github.com/protocolbuffers/protobuf/releases/download/v$ProtocVersion/$ArchiveName"
$RuntimeDirectory = Join-Path $PSScriptRoot 'runtime'
$ArchivePath = Join-Path $RuntimeDirectory $ArchiveName
$ProtocPath = Join-Path $RuntimeDirectory 'bin\protoc.exe'
$WrapperPath = Join-Path $RuntimeDirectory 'bin\protoc-wrapper.exe'
$WrapperSourcePath = Join-Path $PSScriptRoot 'src\protoc_wrapper.rs'

<#
.SYNOPSIS
计算指定文件的 SHA-256，并统一返回大写文本。
#>
function Get-Sha256 {
    param(
        [Parameter(Mandatory)]
        [string] $LiteralPath
    )

    return (Get-FileHash -Algorithm SHA256 -LiteralPath $LiteralPath).Hash.ToUpperInvariant()
}

<#
.SYNOPSIS
确认持久目录中的官方 protoc 与当前固定版本一致。
#>
function Test-ProtocVersion {
    param(
        [Parameter(Mandatory)]
        [string] $LiteralPath
    )

    if (-not (Test-Path -LiteralPath $LiteralPath -PathType Leaf)) {
        return $false
    }

    $VersionOutput = (& $LiteralPath --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "无法执行现有 protoc：$LiteralPath"
    }

    return $VersionOutput -eq "libprotoc $ProtocVersion"
}

New-Item -ItemType Directory -Force -Path $RuntimeDirectory | Out-Null

if (-not (Test-ProtocVersion -LiteralPath $ProtocPath)) {
    if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
        Write-Host "正在下载官方 protoc $ProtocVersion Windows x64 包……"
        Invoke-WebRequest -Uri $DownloadUri -OutFile $ArchivePath
    }

    $ActualSha256 = Get-Sha256 -LiteralPath $ArchivePath
    if ($ActualSha256 -ne $ArchiveSha256) {
        throw "protoc 压缩包 SHA-256 不匹配。文件已保留以便检查：$ArchivePath"
    }

    Write-Host "正在解压官方 protoc 到持久目录……"
    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $RuntimeDirectory -Force

    if (-not (Test-ProtocVersion -LiteralPath $ProtocPath)) {
        throw "安装后的 protoc 版本不是预期的 $ProtocVersion：$ProtocPath"
    }
}

$SourceIsNewer = -not (Test-Path -LiteralPath $WrapperPath -PathType Leaf) -or
    (Get-Item -LiteralPath $WrapperSourcePath).LastWriteTimeUtc -gt
    (Get-Item -LiteralPath $WrapperPath).LastWriteTimeUtc

if ($SourceIsNewer) {
    $RustCompiler = Get-Command rustc -ErrorAction Stop
    Write-Host '正在编译 Windows protoc 兼容 Wrapper……'
    & $RustCompiler.Source --edition=2021 -C opt-level=2 $WrapperSourcePath -o $WrapperPath
    if ($LASTEXITCODE -ne 0) {
        throw "protoc Wrapper 编译失败，退出码：$LASTEXITCODE"
    }
}

$WrapperVersion = (& $WrapperPath --version 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $WrapperVersion -ne "libprotoc $ProtocVersion") {
    throw "protoc Wrapper 验证失败：$WrapperPath"
}

Write-Host "Windows protoc 已就绪并会永久保留：$WrapperPath"
Write-Output $WrapperPath

