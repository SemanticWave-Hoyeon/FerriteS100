# FerriteS100 Release Build Script
# Creates a clean distribution package

param(
    [string]$Version = "0.1.0"
)

$ErrorActionPreference = "Stop"

# Paths
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$TargetDir = "$ProjectRoot\target\release"
$DistDir = "$ProjectRoot\dist"
$ReleaseName = "FerriteS100-v$Version-windows-x64"
$ReleaseDir = "$DistDir\$ReleaseName"

Write-Host "Building FerriteS100 v$Version..." -ForegroundColor Cyan

# 1. Build release
Write-Host "`n[1/5] Compiling release build..." -ForegroundColor Yellow
Set-Location $ProjectRoot
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit 1
}

# 2. Clean and create dist directory
Write-Host "`n[2/5] Preparing distribution folder..." -ForegroundColor Yellow
if (Test-Path $ReleaseDir) {
    Remove-Item -Recurse -Force $ReleaseDir
}
New-Item -ItemType Directory -Force -Path $ReleaseDir | Out-Null

# 3. Copy files
Write-Host "`n[3/5] Copying files..." -ForegroundColor Yellow

# Copy executable
Copy-Item "$TargetDir\ferrite-s100.exe" "$ReleaseDir\FerriteS100.exe"

# Create empty folders with README
$folders = @("Catalogues\FC\S-101", "Catalogues\PC\S-101", "ChartData")
foreach ($folder in $folders) {
    $path = "$ReleaseDir\$folder"
    New-Item -ItemType Directory -Force -Path $path | Out-Null
}

# Create README files for empty folders
@"
Place S-101 Feature Catalogue XML here.
Example: 101_Feature_Catalogue_2.0.0.xml
"@ | Out-File -FilePath "$ReleaseDir\Catalogues\FC\S-101\README.txt" -Encoding UTF8

@"
Place S-101 Portrayal Catalogue files here.
Required structure:
  - portrayal_catalogue.xml
  - Rules/main.lua (and other Lua files)
  - Symbols/*.svg
  - LineStyles/*.xml
  - AreaFills/*.xml
  - ColorProfiles/*.xml
"@ | Out-File -FilePath "$ReleaseDir\Catalogues\PC\S-101\README.txt" -Encoding UTF8

@"
Place S-101 chart files (*.000) here.
"@ | Out-File -FilePath "$ReleaseDir\ChartData\README.txt" -Encoding UTF8

# Copy LICENSE and README
if (Test-Path "$ProjectRoot\LICENSE") {
    Copy-Item "$ProjectRoot\LICENSE" "$ReleaseDir\"
}
Copy-Item "$ProjectRoot\README.md" "$ReleaseDir\"

# 4. Create zip
Write-Host "`n[4/5] Creating zip archive..." -ForegroundColor Yellow
$ZipPath = "$DistDir\$ReleaseName.zip"
if (Test-Path $ZipPath) {
    Remove-Item $ZipPath
}
Compress-Archive -Path $ReleaseDir -DestinationPath $ZipPath

# 5. Summary
Write-Host "`n[5/5] Done!" -ForegroundColor Green
Write-Host ""
Write-Host "Release package created:" -ForegroundColor Cyan
Write-Host "  $ZipPath" -ForegroundColor White
Write-Host ""
Write-Host "Contents:" -ForegroundColor Cyan
Get-ChildItem -Recurse $ReleaseDir | ForEach-Object {
    $indent = "  " * ($_.FullName.Replace($ReleaseDir, "").Split("\").Length - 1)
    if ($_.PSIsContainer) {
        Write-Host "$indent$($_.Name)/" -ForegroundColor Blue
    } else {
        $size = "{0:N0} KB" -f ($_.Length / 1KB)
        Write-Host "$indent$($_.Name) ($size)" -ForegroundColor White
    }
}

Write-Host ""
Write-Host "To test: Unzip and run FerriteS100.exe" -ForegroundColor Gray
