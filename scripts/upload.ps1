# FerriteS100 GitHub Release Upload Script
# Uploads the latest dist package to GitHub Releases using gh CLI

param(
    [string]$Tag = "",
    [switch]$Draft = $false,
    [switch]$Prerelease = $false
)

$ErrorActionPreference = "Stop"

# Paths
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$DistDir = "$ProjectRoot\dist"

# Check if gh CLI is installed
try {
    $null = Get-Command gh -ErrorAction Stop
} catch {
    Write-Host "Error: GitHub CLI (gh) is not installed." -ForegroundColor Red
    Write-Host "Install it from: https://cli.github.com/" -ForegroundColor Yellow
    exit 1
}

# Check if authenticated
$authStatus = gh auth status 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host "Error: Not authenticated with GitHub CLI." -ForegroundColor Red
    Write-Host "Run 'gh auth login' to authenticate." -ForegroundColor Yellow
    exit 1
}

# Find the latest zip in dist folder
Write-Host "Looking for release package in dist/..." -ForegroundColor Cyan
$zips = Get-ChildItem -Path $DistDir -Filter "*.zip" | Sort-Object LastWriteTime -Descending
if ($zips.Count -eq 0) {
    Write-Host "Error: No zip files found in dist/." -ForegroundColor Red
    Write-Host "Run 'scripts\release.ps1' first to create a release package." -ForegroundColor Yellow
    exit 1
}

$latestZip = $zips[0]
Write-Host "Found: $($latestZip.Name)" -ForegroundColor Green

# Extract version from filename (e.g., FerriteS100-v0.0.2-windows-x64.zip)
if ($latestZip.Name -match "FerriteS100-v([0-9.]+)") {
    $version = $matches[1]
} else {
    Write-Host "Warning: Could not extract version from filename. Reading from Cargo.toml..." -ForegroundColor Yellow
    $CargoToml = Get-Content "$ProjectRoot\Cargo.toml" -Raw
    if ($CargoToml -match '\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"') {
        $version = $matches[1]
    } else {
        Write-Host "Error: Could not read version from Cargo.toml" -ForegroundColor Red
        exit 1
    }
}

# Use provided tag or generate from version
if ($Tag -eq "") {
    $Tag = "v$version"
}

Write-Host ""
Write-Host "Release Details:" -ForegroundColor Cyan
Write-Host "  Tag: $Tag" -ForegroundColor White
Write-Host "  File: $($latestZip.FullName)" -ForegroundColor White
Write-Host "  Size: $([math]::Round($latestZip.Length / 1MB, 2)) MB" -ForegroundColor White
Write-Host "  Draft: $Draft" -ForegroundColor White
Write-Host "  Prerelease: $Prerelease" -ForegroundColor White
Write-Host ""

# Confirm
$confirm = Read-Host "Proceed with upload? (y/N)"
if ($confirm -ne "y" -and $confirm -ne "Y") {
    Write-Host "Cancelled." -ForegroundColor Yellow
    exit 0
}

# Build gh release create command
$ghArgs = @("release", "create", $Tag)
$ghArgs += $latestZip.FullName
$ghArgs += "--title"
$ghArgs += "FerriteS100 $Tag"
$ghArgs += "--notes"
$ghArgs += "Release $Tag`n`nSee README.md for installation instructions."

if ($Draft) {
    $ghArgs += "--draft"
}
if ($Prerelease) {
    $ghArgs += "--prerelease"
}

# Check if release already exists
$existingRelease = gh release view $Tag 2>&1
if ($LASTEXITCODE -eq 0) {
    Write-Host "Release $Tag already exists. Uploading asset to existing release..." -ForegroundColor Yellow
    gh release upload $Tag $latestZip.FullName --clobber
} else {
    Write-Host "Creating new release $Tag..." -ForegroundColor Cyan
    & gh $ghArgs
}

if ($LASTEXITCODE -eq 0) {
    Write-Host ""
    Write-Host "Success! Release uploaded." -ForegroundColor Green
    Write-Host ""

    # Get release URL
    $releaseUrl = gh release view $Tag --json url -q ".url" 2>&1
    if ($LASTEXITCODE -eq 0) {
        Write-Host "Release URL: $releaseUrl" -ForegroundColor Cyan
    }
} else {
    Write-Host ""
    Write-Host "Error: Failed to upload release." -ForegroundColor Red
    exit 1
}
