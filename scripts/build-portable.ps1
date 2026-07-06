Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$targetRelease = Join-Path $repoRoot 'target\release'
$portableRoot = Join-Path $repoRoot 'dist\LanPilot-Portable'
$zipPath = Join-Path $repoRoot 'dist\LanPilot-Portable.zip'
$checksumPath = Join-Path $repoRoot 'dist\LanPilot-Portable-checksums.txt'
$releaseInfoPath = Join-Path $repoRoot 'dist\LanPilot-ReleaseInfo.txt'
$releaseChangesPath = Join-Path $repoRoot 'dist\LanPilot-Changes.txt'
$desktopPath = [Environment]::GetFolderPath('Desktop')
$desktopExePath = Join-Path $desktopPath 'LanPilot.exe'

Push-Location $repoRoot
try {
    $commit = git --no-pager rev-parse --short HEAD
    if (-not $commit) {
        throw 'No se pudo obtener el commit actual para metadata de release.'
    }
    $timestamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    $releaseLabel = "LanPilot-$($timestamp.Replace(':','').Replace(' ','_'))-$commit"

    cargo build --release -p lanpilot-app

    if (Test-Path $portableRoot) {
        Remove-Item $portableRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path $portableRoot -Force | Out-Null

    Copy-Item (Join-Path $targetRelease 'lanpilot-app.exe') (Join-Path $portableRoot 'LanPilot.exe')

    @'
LanPilot Portable
=================

1. Ejecuta LanPilot.exe
2. En el equipo remoto pulsa "Compartir mi pantalla"
3. En este equipo pulsa "Buscar equipos", elige por nombre y luego "Conectarme"
4. No necesita instalacion

Este paquete ya es un EXE unico real:
- no requiere _runtime
- no necesita lanpilot-host.exe
- no necesita lanpilot-agent.exe
'@ | Set-Content -Path (Join-Path $portableRoot 'LEEME.txt') -Encoding UTF8

    if (Test-Path $zipPath) {
        Remove-Item $zipPath -Force
    }
    if (Test-Path $checksumPath) {
        Remove-Item $checksumPath -Force
    }
    Compress-Archive -Path (Join-Path $portableRoot '*') -DestinationPath $zipPath -Force
    Copy-Item (Join-Path $portableRoot 'LanPilot.exe') $desktopExePath -Force

    $exeHash = (Get-FileHash -Path (Join-Path $portableRoot 'LanPilot.exe') -Algorithm SHA256).Hash
    $zipHash = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash
    @(
        "release=$releaseLabel"
        "created_at=$timestamp"
        "commit=$commit"
        "LanPilot.exe.sha256=$exeHash"
        "LanPilot-Portable.zip.sha256=$zipHash"
    ) | Set-Content -Path $checksumPath -Encoding UTF8

    @(
        "Release: $releaseLabel"
        "Created at: $timestamp"
        "Commit: $commit"
        "Portable folder: $portableRoot"
        "Portable zip: $zipPath"
        "Desktop EXE: $desktopExePath"
    ) | Set-Content -Path $releaseInfoPath -Encoding UTF8

    git --no-pager log --oneline -15 | Set-Content -Path $releaseChangesPath -Encoding UTF8

    Write-Host ""
    Write-Host "Release label:   $releaseLabel"
    Write-Host "Portable folder: $portableRoot"
    Write-Host "Portable zip:    $zipPath"
    Write-Host "Desktop EXE:     $desktopExePath"
    Write-Host "Checksums:       $checksumPath"
    Write-Host "Release info:    $releaseInfoPath"
    Write-Host "Changes:         $releaseChangesPath"
}
finally {
    Pop-Location
}
