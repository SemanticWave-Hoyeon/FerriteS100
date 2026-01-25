# FerriteS100 Build Script
# Builds main application and plugins in release mode

param(
    [switch]$Debug = $false,
    [switch]$SkipPlugins = $false,
    [switch]$SkipMain = $false
)

$ErrorActionPreference = "Stop"

# Paths
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$PluginsSourceDir = "$ProjectRoot\plugins"
$PluginsOutDir = "$ProjectRoot\plugins_out"
$TargetDir = if ($Debug) { "$ProjectRoot\target\debug" } else { "$ProjectRoot\target\release" }

# Read version from Cargo.toml
$CargoToml = Get-Content "$ProjectRoot\Cargo.toml" -Raw
if ($CargoToml -match '\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
} else {
    Write-Host "Error: Could not read version from Cargo.toml" -ForegroundColor Red
    exit 1
}

$BuildType = if ($Debug) { "Debug" } else { "Release" }
Write-Host "=======================================" -ForegroundColor Cyan
Write-Host "  FerriteS100 Build v$Version ($BuildType)" -ForegroundColor Cyan
Write-Host "=======================================" -ForegroundColor Cyan
Write-Host ""

Set-Location $ProjectRoot

# Step 1: Build main application
if (-not $SkipMain) {
    Write-Host "[1/4] Building main application..." -ForegroundColor Yellow

    if ($Debug) {
        cargo build
    } else {
        cargo build --release
    }

    if ($LASTEXITCODE -ne 0) {
        Write-Host "    Main application build failed!" -ForegroundColor Red
        exit 1
    }
    Write-Host "    Main application built successfully" -ForegroundColor Green
} else {
    Write-Host "[1/4] Skipping main application build" -ForegroundColor Gray
}

# Step 2: Build plugins
if (-not $SkipPlugins) {
    Write-Host ""
    Write-Host "[2/4] Building plugins..." -ForegroundColor Yellow

    # Find all plugin directories
    $PluginDirs = Get-ChildItem -Path $PluginsSourceDir -Directory -ErrorAction SilentlyContinue

    if ($null -eq $PluginDirs -or $PluginDirs.Count -eq 0) {
        Write-Host "    No plugins found in $PluginsSourceDir" -ForegroundColor Gray
    } else {
        foreach ($PluginDir in $PluginDirs) {
            $PluginName = $PluginDir.Name
            $PluginCargoToml = Join-Path $PluginDir.FullName "Cargo.toml"

            if (Test-Path $PluginCargoToml) {
                Write-Host "    Building: $PluginName" -ForegroundColor Cyan

                Set-Location $PluginDir.FullName

                # Temporarily allow errors (cargo outputs warnings to stderr)
                $oldErrorAction = $ErrorActionPreference
                $ErrorActionPreference = "Continue"

                if ($Debug) {
                    cargo build 2>&1 | Out-Null
                } else {
                    cargo build --release 2>&1 | Out-Null
                }
                $buildResult = $LASTEXITCODE

                $ErrorActionPreference = $oldErrorAction

                if ($buildResult -ne 0) {
                    Write-Host "    Plugin '$PluginName' build failed!" -ForegroundColor Red
                    Set-Location $ProjectRoot
                    exit 1
                }

                Write-Host "      Built successfully" -ForegroundColor Green
                Set-Location $ProjectRoot
            }
        }
    }
} else {
    Write-Host "[2/4] Skipping plugins build" -ForegroundColor Gray
}

# Step 3: Deploy plugins to plugins_out
if (-not $SkipPlugins) {
    Write-Host ""
    Write-Host "[3/4] Deploying plugins to plugins_out..." -ForegroundColor Yellow

    # Create plugins_out if not exists
    if (-not (Test-Path $PluginsOutDir)) {
        New-Item -ItemType Directory -Force -Path $PluginsOutDir | Out-Null
    }

    $PluginDirs = Get-ChildItem -Path $PluginsSourceDir -Directory -ErrorAction SilentlyContinue

    foreach ($PluginDir in $PluginDirs) {
        $PluginName = $PluginDir.Name
        $DllName = $PluginName -replace "-", "_"

        $SourceDll = if ($Debug) {
            "$($PluginDir.FullName)\target\debug\$DllName.dll"
        } else {
            "$($PluginDir.FullName)\target\release\$DllName.dll"
        }

        $ManifestSource = Join-Path $PluginDir.FullName "manifest.json"
        $OutputDir = "$PluginsOutDir\$PluginName"

        if (-not (Test-Path $SourceDll)) {
            Write-Host "    Warning: DLL not found for $PluginName" -ForegroundColor Yellow
            continue
        }

        # Create output directory
        if (-not (Test-Path $OutputDir)) {
            New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
        }

        # Copy DLL
        Copy-Item $SourceDll "$OutputDir\$DllName.dll" -Force
        Write-Host "    Deployed: $PluginName/$DllName.dll" -ForegroundColor Gray

        # Copy and update manifest.json with correct hash
        if (Test-Path $ManifestSource) {
            # Calculate SHA-256 hash of DLL
            $DllHash = (Get-FileHash -Path $SourceDll -Algorithm SHA256).Hash.ToLower()

            # Read manifest and update hash
            $ManifestContent = Get-Content $ManifestSource -Raw | ConvertFrom-Json
            $ManifestContent.dll_hash = $DllHash

            # Write updated manifest (UTF-8 without BOM)
            $JsonOutput = $ManifestContent | ConvertTo-Json -Depth 10
            [System.IO.File]::WriteAllText("$OutputDir\manifest.json", $JsonOutput, [System.Text.UTF8Encoding]::new($false))
            Write-Host "    Updated:  $PluginName/manifest.json (hash: $($DllHash.Substring(0,16))...)" -ForegroundColor Gray
        } else {
            Write-Host "    Warning: No manifest.json found for $PluginName" -ForegroundColor Yellow
        }
    }

    Write-Host "    Plugins deployed successfully" -ForegroundColor Green

    # Also copy to target directory (exe directory)
    $TargetPluginsDir = "$TargetDir\plugins_out"
    if (Test-Path $PluginsOutDir) {
        if (Test-Path $TargetPluginsDir) {
            Remove-Item -Path $TargetPluginsDir -Recurse -Force
        }
        Copy-Item -Path $PluginsOutDir -Destination $TargetPluginsDir -Recurse -Force
        Write-Host "    Copied to: $TargetPluginsDir" -ForegroundColor Gray
    }
} else {
    Write-Host "[3/4] Skipping plugin deployment" -ForegroundColor Gray
}

# Step 4: Copy Catalogues to target directory
Write-Host ""
Write-Host "[4/4] Copying Catalogues to target directory..." -ForegroundColor Yellow

$CataloguesSource = "$ProjectRoot\Catalogues"
$CataloguesTarget = "$TargetDir\Catalogues"

if (Test-Path $CataloguesSource) {
    if (Test-Path $CataloguesTarget) {
        Remove-Item -Path $CataloguesTarget -Recurse -Force
    }
    Copy-Item -Path $CataloguesSource -Destination $CataloguesTarget -Recurse -Force
    Write-Host "    Catalogues copied to: $CataloguesTarget" -ForegroundColor Green
} else {
    Write-Host "    Warning: Catalogues folder not found at $CataloguesSource" -ForegroundColor Yellow
}

# Also copy logs folder structure
$LogsTarget = "$TargetDir\logs"
if (-not (Test-Path $LogsTarget)) {
    New-Item -ItemType Directory -Force -Path $LogsTarget | Out-Null
    Write-Host "    Created logs folder: $LogsTarget" -ForegroundColor Gray
}

# Summary
Write-Host ""
Write-Host "=======================================" -ForegroundColor Cyan
Write-Host "  Build Complete!" -ForegroundColor Green
Write-Host "=======================================" -ForegroundColor Cyan
Write-Host ""
Write-Host "Output locations:" -ForegroundColor White
if (-not $SkipMain) {
    Write-Host "  Main app: $TargetDir\ferrite-s100.exe" -ForegroundColor Gray
}
if (-not $SkipPlugins) {
    Write-Host "  Plugins:  $PluginsOutDir\" -ForegroundColor Gray

    # List deployed plugins
    $DeployedPlugins = Get-ChildItem -Path $PluginsOutDir -Directory -ErrorAction SilentlyContinue
    foreach ($Plugin in $DeployedPlugins) {
        $DllFiles = Get-ChildItem -Path $Plugin.FullName -Filter "*.dll" -ErrorAction SilentlyContinue
        if ($DllFiles) {
            Write-Host "            - $($Plugin.Name)" -ForegroundColor Gray
        }
    }
}
Write-Host ""
Write-Host "To run: .\target\release\ferrite-s100.exe" -ForegroundColor Cyan
Write-Host ""
