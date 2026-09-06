<#
    Builds a portable Vox folder and zips it, so someone can run Vox without installing
    a Rust toolchain. Everything lands in dist\ (git-ignored).

    Usage:  scripts\package.ps1 [-Model base.en-q5_1] [-SkipBuild]
#>
param(
    [string]$Model = "base.en-q5_1",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

if (-not $SkipBuild) {
    Write-Host "Building release binaries..." -ForegroundColor Cyan
    & cmd /c "scripts\cargo-msvc.cmd build --release -p voxd"
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
}

$modelFile = "ggml-$Model.bin"
$modelPath = Join-Path $repo "models\$modelFile"
if (-not (Test-Path $modelPath)) {
    throw "$modelPath not found. Download it from https://huggingface.co/ggerganov/whisper.cpp"
}

$dist = Join-Path $repo "dist\Vox-portable"
Remove-Item (Join-Path $repo "dist") -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $dist "models") | Out-Null

Copy-Item "target\release\voxd.exe", "target\release\vox.exe" $dist
Copy-Item $modelPath (Join-Path $dist "models")
Copy-Item (Join-Path $PSScriptRoot "portable-README.txt") (Join-Path $dist "README.txt")

$zip = Join-Path $repo "dist\Vox-portable.zip"
Compress-Archive -Path "$dist\*" -DestinationPath $zip -CompressionLevel Optimal -Force

$mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "`nPackaged $zip ($mb MB) with model $Model" -ForegroundColor Green
