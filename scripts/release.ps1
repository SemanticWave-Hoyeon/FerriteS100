# FerriteS100 Release Build Script
# Creates a clean distribution package with plugins

param(
    [string]$Version = "",
    [switch]$KMOU = $false,
    [switch]$SkipPlugins = $false
)

$ErrorActionPreference = "Stop"

# Paths
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$PluginsDir = "$ProjectRoot\plugins"

# Read version from Cargo.toml if not provided
if ($Version -eq "") {
    $CargoToml = Get-Content "$ProjectRoot\Cargo.toml" -Raw
    if ($CargoToml -match '\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"') {
        $Version = $matches[1]
    } else {
        Write-Host "Error: Could not read version from Cargo.toml" -ForegroundColor Red
        exit 1
    }
}
$TargetDir = "$ProjectRoot\target\release"
$DistDir = "$ProjectRoot\dist"
$ReleasePrefix = if ($KMOU) { "kmou_" } else { "" }
$ReleaseName = "${ReleasePrefix}FerriteS100-v$Version-windows-x64"
$ReleaseDir = "$DistDir\$ReleaseName"

$BuildType = if ($KMOU) { "KMOU Edition" } else { "Standard" }
Write-Host "=======================================" -ForegroundColor Cyan
Write-Host "  FerriteS100 Release v$Version" -ForegroundColor Cyan
Write-Host "  $BuildType" -ForegroundColor Cyan
Write-Host "=======================================" -ForegroundColor Cyan

# 1. Build main application
Write-Host "`n[1/6] Compiling main application..." -ForegroundColor Yellow
Set-Location $ProjectRoot
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit 1
}

# 2. Build plugins
if (-not $SkipPlugins) {
    Write-Host "`n[2/6] Building plugins..." -ForegroundColor Yellow
    $PluginSubDirs = Get-ChildItem -Path $PluginsDir -Directory -ErrorAction SilentlyContinue

    foreach ($PluginSubDir in $PluginSubDirs) {
        $PluginName = $PluginSubDir.Name
        $PluginCargoToml = Join-Path $PluginSubDir.FullName "Cargo.toml"

        if (Test-Path $PluginCargoToml) {
            Write-Host "  Building: $PluginName" -ForegroundColor Gray
            Set-Location $PluginSubDir.FullName
            cargo build --release
            if ($LASTEXITCODE -ne 0) {
                Write-Host "  Plugin build failed: $PluginName" -ForegroundColor Red
                exit 1
            }
            Set-Location $ProjectRoot
        }
    }
} else {
    Write-Host "`n[2/6] Skipping plugin build..." -ForegroundColor Gray
}

# 3. Clean and create dist directory
Write-Host "`n[3/6] Preparing distribution folder..." -ForegroundColor Yellow
if (Test-Path $ReleaseDir) {
    Remove-Item -Recurse -Force $ReleaseDir
}
New-Item -ItemType Directory -Force -Path $ReleaseDir | Out-Null

# 4. Copy files
Write-Host "`n[4/6] Copying files..." -ForegroundColor Yellow

# Copy executable
Copy-Item "$TargetDir\ferrite-s100.exe" "$ReleaseDir\FerriteS100.exe"
Write-Host "  Copied: FerriteS100.exe" -ForegroundColor Gray

if ($KMOU) {
    # KMOU Edition: Copy actual Catalogues and ChartData
    Write-Host "  Copying Catalogues (S-101 & S-421)..." -ForegroundColor Gray
    if (Test-Path "$ProjectRoot\Catalogues") {
        Copy-Item -Recurse "$ProjectRoot\Catalogues" "$ReleaseDir\Catalogues"
    } else {
        Write-Host "  Warning: Catalogues directory not found" -ForegroundColor Yellow
    }

    Write-Host "  Copying ChartData..." -ForegroundColor Gray
    if (Test-Path "$ProjectRoot\ChartData") {
        Copy-Item -Recurse "$ProjectRoot\ChartData" "$ReleaseDir\ChartData"
    } else {
        Write-Host "  Warning: ChartData directory not found" -ForegroundColor Yellow
    }
} else {
    # Standard Edition: Create empty folders with README
    $folders = @(
        "Catalogues\FC\S-101",
        "Catalogues\PC\S-101",
        "Catalogues\FC\S-421",
        "Catalogues\PC\S-421",
        "ChartData"
    )
    foreach ($folder in $folders) {
        $path = "$ReleaseDir\$folder"
        New-Item -ItemType Directory -Force -Path $path | Out-Null
    }

    # Create README files
@"
Place S-101 Feature Catalogue XML here.
Example: S-101_FC_1.2.0.xml
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
Place S-421 Feature Catalogue XML here.
Example: S-421_FC_1.0.0.xml

Required for Route Plugin functionality.
"@ | Out-File -FilePath "$ReleaseDir\Catalogues\FC\S-421\README.txt" -Encoding UTF8

@"
Place S-421 Portrayal Catalogue files here.
Required structure:
  - portrayal_catalogue.xml
  - Symbols/*.svg (RTEWPT01.svg, RTEWPT02.svg, etc.)
  - LineStyles/*.xml
  - ColorProfiles/*.xml

Required for Route Plugin functionality.
"@ | Out-File -FilePath "$ReleaseDir\Catalogues\PC\S-421\README.txt" -Encoding UTF8

@"
Place S-101 chart files (*.000) here.
"@ | Out-File -FilePath "$ReleaseDir\ChartData\README.txt" -Encoding UTF8
}

# Copy plugins
if (-not $SkipPlugins) {
    Write-Host "  Copying plugins..." -ForegroundColor Gray
    $PluginOutDir = "$ReleaseDir\plugins"
    New-Item -ItemType Directory -Force -Path $PluginOutDir | Out-Null

    foreach ($PluginSubDir in $PluginSubDirs) {
        $PluginName = $PluginSubDir.Name
        $DllName = $PluginName -replace "-", "_"
        $SourceDll = "$PluginSubDir\target\release\$DllName.dll"
        $ManifestPath = "$PluginSubDir\manifest.json"

        if (Test-Path $SourceDll) {
            $PluginDestDir = "$PluginOutDir\$PluginName"
            New-Item -ItemType Directory -Force -Path $PluginDestDir | Out-Null

            Copy-Item $SourceDll "$PluginDestDir\$DllName.dll"
            Write-Host "    Copied: $PluginName/$DllName.dll" -ForegroundColor Gray

            if (Test-Path $ManifestPath) {
                # Read manifest and update hash
                $ManifestContent = Get-Content $ManifestPath -Raw | ConvertFrom-Json

                # Calculate SHA-256 hash of DLL
                $DllHash = (Get-FileHash -Path $SourceDll -Algorithm SHA256).Hash.ToLower()
                $ManifestContent.dll_hash = $DllHash

                # Write updated manifest
                $ManifestContent | ConvertTo-Json -Depth 10 | Out-File "$PluginDestDir\manifest.json" -Encoding UTF8
                Write-Host "    Copied: $PluginName/manifest.json (hash updated)" -ForegroundColor Gray
            }
        }
    }
}

# Copy LICENSE and README
if (Test-Path "$ProjectRoot\LICENSE") {
    Copy-Item "$ProjectRoot\LICENSE" "$ReleaseDir\"
}
Copy-Item "$ProjectRoot\README.md" "$ReleaseDir\"

# 5. Create zip
Write-Host "`n[5/6] Creating zip archive..." -ForegroundColor Yellow
$ZipPath = "$DistDir\$ReleaseName.zip"
if (Test-Path $ZipPath) {
    Remove-Item $ZipPath
}
Compress-Archive -Path $ReleaseDir -DestinationPath $ZipPath

# 6. Summary
Write-Host "`n[6/6] Done!" -ForegroundColor Green
Write-Host ""
Write-Host "Release package created:" -ForegroundColor Cyan
Write-Host "  $ZipPath" -ForegroundColor White

# Get zip size
$ZipSize = (Get-Item $ZipPath).Length
$ZipSizeMB = "{0:N2} MB" -f ($ZipSize / 1MB)
Write-Host "  Size: $ZipSizeMB" -ForegroundColor Gray

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

# Clean up source folder
Write-Host "`nCleaning up..." -ForegroundColor Gray
Remove-Item -Recurse -Force $ReleaseDir

Write-Host ""
Write-Host "To test: Unzip and run FerriteS100.exe" -ForegroundColor Gray
Write-Host ""
