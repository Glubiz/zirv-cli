# Issue #615 (N23): Windows counterpart of bench-build-cost.sh -- same
# contract, same output shape. Measures clean debug build time, clean
# release build time, and stripped release binary size for two git refs,
# and fails when either regresses past the documented threshold.
# Thresholds and the actual measurements recorded from this machine live in
# docs/benchmarks/build-cost.md -- read that first if a number here looks
# surprising.
#
# Usage:
#   powershell -File scripts/bench-build-cost.ps1 [-BaseRef main] [-HeadRef HEAD]
#
# Env overrides (same names as the sh script):
#   ZIRV_BENCH_BUILD_THRESHOLD_PCT   max allowed clean-build regression, %  (default 25)
#   ZIRV_BENCH_SIZE_THRESHOLD_PCT    max allowed release-size regression, % (default 15)
#   ZIRV_BENCH_SKIP_RELEASE=1        skip the (slow, LTO) release build/size measurement
param(
    [string]$BaseRef = "main",
    [string]$HeadRef = "HEAD"
)

$ErrorActionPreference = "Stop"

# This machine's PATH under a plain PowerShell invocation does not reliably
# carry `cargo` (see the repo's own CLAUDE.md); rebuild it from the Machine/
# User scopes the same way the harness's own cargo wrapper does before
# calling cargo anywhere below.
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = "3" }

$BuildThresholdPct = 25.0
if ($env:ZIRV_BENCH_BUILD_THRESHOLD_PCT) { $BuildThresholdPct = [double]$env:ZIRV_BENCH_BUILD_THRESHOLD_PCT }
$SizeThresholdPct = 15.0
if ($env:ZIRV_BENCH_SIZE_THRESHOLD_PCT) { $SizeThresholdPct = [double]$env:ZIRV_BENCH_SIZE_THRESHOLD_PCT }
$SkipRelease = $env:ZIRV_BENCH_SKIP_RELEASE -eq "1"

$repoRoot = (git rev-parse --show-toplevel).Trim()
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("zirv-bench-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $work | Out-Null

function Remove-Worktrees {
    foreach ($label in @("base", "head")) {
        $wt = Join-Path $work $label
        if (Test-Path $wt) {
            git -C $repoRoot worktree remove --force $wt 2>$null | Out-Null
        }
    }
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

function Measure-Ref {
    param([string]$Ref, [string]$Label)

    $wt = Join-Path $work $Label
    git -C $repoRoot worktree add --quiet --detach $wt $Ref 2>&1 | Write-Host
    $target = Join-Path $work ("target-" + $Label)
    $env:CARGO_TARGET_DIR = $target

    $d0 = Get-Date
    Push-Location $wt
    cargo build --bin zirv --quiet 2>&1 | Write-Host
    Pop-Location
    $d1 = Get-Date
    $debugSecs = [math]::Round(($d1 - $d0).TotalSeconds, 2)

    $releaseSecs = "skipped"
    $strippedBytes = "skipped"
    if (-not $SkipRelease) {
        $r0 = Get-Date
        Push-Location $wt
        cargo build --release --bin zirv --quiet 2>&1 | Write-Host
        Pop-Location
        $r1 = Get-Date
        $releaseSecs = [math]::Round(($r1 - $r0).TotalSeconds, 2)

        $bin = Join-Path $target "release\zirv.exe"
        if (-not (Test-Path $bin)) { $bin = Join-Path $target "release\zirv" }
        $strippedPath = Join-Path $work ($Label + "-stripped.exe")
        Copy-Item $bin $strippedPath -Force
        # No universal Windows "strip"; a release build here already has
        # `debug = false` in [profile.release] (Cargo.toml), so this is the
        # PE's own on-disk size with no separate debug info to strip. If
        # llvm-strip is on PATH (e.g. via the LLVM toolchain), use it for a
        # stricter measurement; otherwise report the as-built size and say so.
        $llvmStrip = Get-Command "llvm-strip" -ErrorAction SilentlyContinue
        if ($llvmStrip) {
            & $llvmStrip.Source $strippedPath 2>$null
        }
        $strippedBytes = (Get-Item $strippedPath).Length
    }

    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    $sha = (git -C $repoRoot rev-parse --short $Ref).Trim()
    git -C $repoRoot worktree remove --force $wt 2>$null | Out-Null

    [pscustomobject]@{
        Label         = $Label
        Ref           = $Ref
        Sha           = $sha
        DebugSecs     = $debugSecs
        ReleaseSecs   = $releaseSecs
        StrippedBytes = $strippedBytes
    }
}

try {
    Write-Host "Measuring base ($BaseRef)..."
    $base = Measure-Ref -Ref $BaseRef -Label "base"
    Write-Host "Measuring head ($HeadRef)..."
    $head = Measure-Ref -Ref $HeadRef -Label "head"

    "base ref=$($base.Ref) sha=$($base.Sha) debug_secs=$($base.DebugSecs) release_secs=$($base.ReleaseSecs) stripped_bytes=$($base.StrippedBytes)"
    "head ref=$($head.Ref) sha=$($head.Sha) debug_secs=$($head.DebugSecs) release_secs=$($head.ReleaseSecs) stripped_bytes=$($head.StrippedBytes)"

    $debugPct = if ($base.DebugSecs -eq 0) { 0 } else { [math]::Round((($head.DebugSecs - $base.DebugSecs) / $base.DebugSecs) * 100, 1) }
    Write-Host "clean debug build: $($base.DebugSecs)s -> $($head.DebugSecs)s ($debugPct%)"

    # `Write-Error` under `$ErrorActionPreference = "Stop"` is a TERMINATING
    # error -- it would abort the script before the size check below ever
    # ran, silently dropping that half of the report. Both checks must run
    # and both must be reported regardless of which one(s) fail, matching
    # bench-build-cost.sh's own behaviour (it sets a status flag and keeps
    # going), so failures are written to stderr directly instead.
    $failed = $false
    if ($debugPct -gt $BuildThresholdPct) {
        [Console]::Error.WriteLine("FAIL: clean debug build time regressed $debugPct% (threshold $BuildThresholdPct%)")
        $failed = $true
    }

    if ($base.StrippedBytes -ne "skipped" -and $head.StrippedBytes -ne "skipped") {
        $sizePct = if ($base.StrippedBytes -eq 0) { 0 } else { [math]::Round((($head.StrippedBytes - $base.StrippedBytes) / $base.StrippedBytes) * 100, 1) }
        Write-Host "release binary size: $($base.StrippedBytes)B -> $($head.StrippedBytes)B ($sizePct%)"
        if ($sizePct -gt $SizeThresholdPct) {
            [Console]::Error.WriteLine("FAIL: release binary size regressed $sizePct% (threshold $SizeThresholdPct%)")
            $failed = $true
        }
    }

    if ($failed) { exit 1 }
    exit 0
}
finally {
    Remove-Worktrees
}
