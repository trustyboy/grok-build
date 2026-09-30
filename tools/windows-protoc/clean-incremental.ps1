[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateRange(1, 3650)]
    [int] $OlderThanDays = 14,

    [ValidateRange(2, 100)]
    [int] $KeepLatest = 2,

    [switch] $Apply
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 只扫描普通目录；包含链接的缓存整体保留，避免跨目录删除。
function Get-CacheState {
    param([Parameter(Mandatory)][string] $LiteralPath)

    $size = 0L
    $latest = [datetime]::MinValue
    $pending = [Collections.Generic.Stack[IO.DirectoryInfo]]::new()
    $pending.Push([IO.DirectoryInfo]::new($LiteralPath))
    while ($pending.Count -gt 0) {
        $directory = $pending.Pop()
        if ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $null }
        if ($directory.LastWriteTimeUtc -gt $latest) { $latest = $directory.LastWriteTimeUtc }
        foreach ($entry in $directory.GetFileSystemInfos()) {
            if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $null }
            if ($entry.LastWriteTimeUtc -gt $latest) { $latest = $entry.LastWriteTimeUtc }
            if ($entry -is [IO.DirectoryInfo]) {
                $pending.Push($entry)
            }
            else {
                $size += $entry.Length
            }
        }
    }
    [pscustomobject]@{ Bytes = $size; LatestUtc = $latest }
}

$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$ReleaseRoot = Join-Path $RepositoryRoot 'target\release'
$IncrementalRoot = Join-Path $ReleaseRoot 'incremental'
$Cutoff = [datetime]::UtcNow.AddDays(-$OlderThanDays)
$CargoLock = $null

if (-not (Test-Path -LiteralPath $IncrementalRoot -PathType Container)) {
    Write-Host '没有 Release 增量缓存。'
    return
}

# 固定清理范围，不跟随 target/release/incremental 任一层的目录链接。
foreach ($path in @((Join-Path $RepositoryRoot 'target'), $ReleaseRoot, $IncrementalRoot)) {
    if ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "清理路径含链接，已停止：$path"
    }
}

try {
    if ($Apply) {
        if (Get-Process -Name cargo, rustc -ErrorAction SilentlyContinue) {
            throw '检测到 Cargo 或 rustc 正在运行，请等待构建结束后清理。'
        }
        # 独占 Cargo 的产物锁，避免扫描和删除期间启动新的构建。
        try {
            $CargoLock = [IO.File]::Open(
                (Join-Path $ReleaseRoot '.cargo-lock'),
                [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        }
        catch {
            throw '无法独占 Release 构建锁，请等待构建结束后重试。'
        }
    }

    $caches = @(foreach ($directory in Get-ChildItem -LiteralPath $IncrementalRoot -Directory -Force) {
        if ($directory.Name -notmatch '^(?<crate>.+)-[a-z0-9]+$') { continue }
        $crate = $Matches['crate']
        # build_script_build 被多个不同包共用，无法按名称识别变体。
        if ($crate -eq 'build_script_build') { continue }
        $state = Get-CacheState -LiteralPath $directory.FullName
        if ($null -eq $state) {
            Write-Warning "缓存含链接，已保留：$($directory.FullName)"
            continue
        }
        [pscustomobject]@{
            Crate = $crate
            Path = $directory.FullName
            Bytes = $state.Bytes
            LatestUtc = $state.LatestUtc
        }
    })
    $candidates = @(foreach ($group in $caches | Group-Object Crate) {
        $ordered = @($group.Group | Sort-Object LatestUtc -Descending)
        $ordered | Select-Object -Skip $KeepLatest | Where-Object LatestUtc -LT $Cutoff
    })
    $total = 0L
    foreach ($candidate in $candidates) { $total += $candidate.Bytes }
    Write-Host ("候选目录 {0} 个，逻辑大小 {1:N2} GiB；每个 crate 至少保留最新 {2} 个变体。" -f
        $candidates.Count, ($total / 1GB), $KeepLatest)
    $candidates | Sort-Object Bytes -Descending |
        Select-Object @{Name='GiB'; Expression={[math]::Round($_.Bytes / 1GB, 3)}}, LatestUtc, Path |
        Format-Table -AutoSize | Out-Host
    if (-not $Apply) {
        Write-Host '当前为预览。确认清单后传入 -Apply 执行；修改时间不能证明缓存已失效，旧变体再次使用时会重编译。'
        return
    }

    $removed = 0
    $removedBytes = 0L
    foreach ($candidate in $candidates) {
        if (-not $PSCmdlet.ShouldProcess($candidate.Path, '删除过期增量缓存')) { continue }
        $current = Get-CacheState -LiteralPath $candidate.Path
        if ($null -eq $current -or $current.LatestUtc -ne $candidate.LatestUtc -or $current.Bytes -ne $candidate.Bytes) {
            throw "缓存发生变化，已停止清理：$($candidate.Path)"
        }
        Remove-Item -LiteralPath $candidate.Path -Recurse -Force
        $removed++
        $removedBytes += $candidate.Bytes
    }
    Write-Host ("已删除 {0} 个目录，逻辑大小 {1:N2} GiB；实际释放空间以磁盘可用容量变化为准。" -f
        $removed, ($removedBytes / 1GB))
}
finally {
    if ($null -ne $CargoLock) { $CargoLock.Dispose() }
}
