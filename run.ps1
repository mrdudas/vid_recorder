# Start vid_recorder (Windows) with the latest version.
#
# - In a git checkout with Rust installed (development machine): git pull,
#   rebuild if anything changed, run.
# - Otherwise (installed from a release archive): download the newest
#   GitHub release if it differs from .\VERSION, unpack it here, run.
#
# Arguments are passed on, e.g. .\run.ps1 record-test 5
# No network or a failed update: starts the current version.

$Repo = "mrdudas/vid_recorder"
$Asset = "vid_recorder-windows-x86_64"
Set-Location $PSScriptRoot

if ((Test-Path .git) -and (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:GIT_TERMINAL_PROMPT = "0"
    git diff --quiet; $dirty = $LASTEXITCODE -ne 0
    git diff --cached --quiet; $dirty = $dirty -or ($LASTEXITCODE -ne 0)
    if ($dirty) {
        Write-Warning "update: local changes, not pulling"
    } else {
        git -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=10 pull --ff-only --quiet
        if ($LASTEXITCODE -ne 0) { Write-Warning "update: git pull failed, starting the current version" }
    }
    # Incremental: does nothing if no source changed.
    cargo build --release --quiet
    $exe = "target\release\vid_recorder.exe"
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "build failed"
        if (-not (Test-Path $exe)) { exit 1 }
        Write-Warning "starting the previous build"
    }
} else {
    $exe = ".\vid_recorder.exe"
    try {
        $latest = (Invoke-RestMethod -TimeoutSec 10 "https://api.github.com/repos/$Repo/releases/latest").tag_name
    } catch {
        $latest = $null
        Write-Warning "update: cannot reach GitHub, starting the current version"
    }
    $current = if (Test-Path VERSION) { (Get-Content VERSION -Raw).Trim() } else { "" }
    if ($latest -and $latest -ne $current) {
        Write-Host "update: downloading $latest"
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
        try {
            New-Item -ItemType Directory $tmp | Out-Null
            Invoke-WebRequest -TimeoutSec 300 -OutFile "$tmp\a.zip" `
                "https://github.com/$Repo/releases/download/$latest/$Asset.zip"
            Expand-Archive "$tmp\a.zip" -DestinationPath "$tmp\x"
            Copy-Item -Recurse -Force "$tmp\x\*" .
            Write-Host "update: now $latest"
        } catch {
            Write-Warning "update: download failed, starting the current version"
        } finally {
            Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
        }
    }
    if (-not (Test-Path $exe)) { Write-Error "vid_recorder.exe not found"; exit 1 }
}

& $exe @args
exit $LASTEXITCODE
