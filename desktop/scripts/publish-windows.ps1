<#
.SYNOPSIS
  Publish a built Windows installer as the public download on consistent.ceo.

.DESCRIPTION
  The marketing site (github.com/Al-Baqshi/Consistent, deployed on Vercel)
  serves early-access installers from its own public/downloads/ folder, the
  same way the macOS DMG ships. Its download page turns the Windows card into a
  real button as soon as public/downloads/Consistent-CEO-Windows-Setup.exe
  exists, so publishing is: copy the file in under that stable name, commit,
  push. Vercel redeploys on the push.

  Run it AFTER build-windows.ps1 and AFTER the smoke test in
  docs/windows-desktop.md has passed on this machine — whatever is pushed is
  what the public downloads.

    powershell -ExecutionPolicy Bypass -File desktop\scripts\publish-windows.ps1

.PARAMETER Installer
  The setup .exe to publish. Default: the newest NSIS installer under
  desktop\src-tauri\target\release\bundle\nsis.

.PARAMETER SiteRepo
  Where the site repository is (or will be cloned). Default: %USERPROFILE%\consistent-site.

.PARAMETER NoPush
  Commit locally but do not push (nothing goes live).

.PARAMETER Yes
  Skip the "has the smoke test passed?" confirmation.
#>
[CmdletBinding()]
param(
    [string]$Installer,
    [string]$SiteRepo = (Join-Path $env:USERPROFILE "consistent-site"),
    [string]$SiteRemote = "https://github.com/Al-Baqshi/Consistent.git",
    [switch]$NoPush,
    [switch]$Yes
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$AssetName = "Consistent-CEO-Windows-Setup.exe"
$PublicUrl = "https://www.consistent.ceo/downloads/$AssetName"

function Write-Step([string]$Message) { Write-Host "`n==> $Message" -ForegroundColor Cyan }

function Invoke-Git {
    & git @args
    if ($LASTEXITCODE -ne 0) { throw "git $($args -join ' ') failed" }
}

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    throw "git is not installed (winget install Git.Git)."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

Write-Step "Finding the installer"
if (-not $Installer) {
    $nsisDir = Join-Path $repoRoot "desktop\src-tauri\target\release\bundle\nsis"
    $found = Get-ChildItem -Path $nsisDir -Filter "*-setup.exe" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $found) { throw "No installer under $nsisDir. Run desktop\scripts\build-windows.ps1 first." }
    $Installer = $found.FullName
}
if (-not (Test-Path $Installer)) { throw "Installer not found: $Installer" }
$item = Get-Item $Installer
$sizeMb = [math]::Round($item.Length / 1MB, 1)
Write-Host "  $($item.FullName)"
Write-Host "  $sizeMb MB, built $($item.LastWriteTime)"

# GitHub refuses files over 100 MB outright; the site cannot host it that way.
if ($item.Length -ge 100MB) {
    throw "The installer is $sizeMb MB. GitHub rejects files of 100 MB or more, so it cannot be committed to the site repo; host it in a bucket and set NEXT_PUBLIC_DOWNLOAD_BASE instead."
}

$signatureStatus = "unknown"
if (Get-Command Get-AuthenticodeSignature -ErrorAction SilentlyContinue) {
    $signatureStatus = (Get-AuthenticodeSignature $item.FullName).Status
}
Write-Host "  Signature: $signatureStatus"

if (-not $Yes) {
    Write-Host ""
    Write-Host "This makes the installer publicly downloadable at $PublicUrl" -ForegroundColor Yellow
    $answer = Read-Host "Has the smoke test in docs\windows-desktop.md passed with THIS build? (y/N)"
    if ($answer -notmatch '^(y|yes)$') {
        Write-Host "Not published."
        exit 1
    }
}

Write-Step "Preparing the site repository ($SiteRepo)"
if (-not (Test-Path (Join-Path $SiteRepo ".git"))) {
    Invoke-Git clone $SiteRemote $SiteRepo
}
Push-Location $SiteRepo
try {
    $dirty = & git status --porcelain
    if ($dirty) {
        throw "The site repository at $SiteRepo has uncommitted changes. Commit or discard them, or pass -SiteRepo pointing at a clean clone."
    }
    Invoke-Git checkout main
    Invoke-Git pull --rebase origin main

    if (-not (& git config user.email)) {
        throw "git has no user.email here. Run: git config --global user.email you@example.com (and user.name)."
    }

    Write-Step "Committing $AssetName"
    $downloads = Join-Path $SiteRepo "public\downloads"
    New-Item -ItemType Directory -Force -Path $downloads | Out-Null
    Copy-Item -Force $item.FullName (Join-Path $downloads $AssetName)
    Invoke-Git add -- "public/downloads/$AssetName"
    & git diff --cached --quiet
    if ($LASTEXITCODE -eq 0) {
        Write-Host "That exact installer is already published; nothing to do."
        exit 0
    }
    $version = $item.BaseName
    Invoke-Git commit -m "feat(download): publish the Windows early-access installer" -m "Built from $version ($sizeMb MB, signature: $signatureStatus)."

    if ($NoPush) {
        Write-Host "`nCommitted locally (-NoPush). Push from $SiteRepo when ready." -ForegroundColor Yellow
        exit 0
    }
    Write-Step "Pushing (Vercel redeploys the site)"
    Invoke-Git push origin main
} finally { Pop-Location }

Write-Step "Done"
Write-Host "In a few minutes, once Vercel has deployed:"
Write-Host "  $PublicUrl"
Write-Host "  https://www.consistent.ceo/download   (the Windows card becomes a download button)"
