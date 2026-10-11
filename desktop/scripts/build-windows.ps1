<#
.SYNOPSIS
  Build the Windows installer on this machine: check (or install) the build
  tools, then produce Consistent-CEO's NSIS setup.exe.

.DESCRIPTION
  The installer cannot be built on a Mac — vendored OpenSSL and whisper.cpp
  compile from C sources with MSVC — and CI may not be available, so this is
  the one-command route on a Windows 10/11 x64 machine.

  Run from anywhere in the repository:

    powershell -ExecutionPolicy Bypass -File desktop\scripts\build-windows.ps1 -InstallPrerequisites

  -InstallPrerequisites installs whatever is missing with winget (it asks
  Windows for elevation where an installer needs it). Without it the script
  only reports what is missing and stops.

.NOTES
  What each tool is for:
    Visual Studio Build Tools (C++)  MSVC compiler + Windows SDK for every native crate
    Rust (rustup)                    the toolchain pinned in rust-toolchain.toml
    Node.js LTS                      the dashboard (sveltfront) and the Tauri CLI
    CMake                            whisper.cpp, the dictation engine
    LLVM                             libclang, which generates whisper.cpp's Rust bindings
    Strawberry Perl + NASM           vendored OpenSSL (the gateway's WebAuthn dependency)
    Git for Windows                  cloning, and at runtime the Claude CLI needs its bash
#>
[CmdletBinding()]
param(
    [switch]$InstallPrerequisites,
    # NSIS is what releases ship; "msi" additionally needs WiX.
    [string]$Bundles = "nsis"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Write-Step([string]$Message) { Write-Host "`n==> $Message" -ForegroundColor Cyan }
function Write-Ok([string]$Message) { Write-Host "  ok  $Message" -ForegroundColor Green }
function Write-Missing([string]$Message) { Write-Host "  --  $Message" -ForegroundColor Yellow }

# A fresh winget install updates the machine PATH, not this session's.
function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $user = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = "$machine;$user"
}

function Add-ToPathIfPresent([string]$Dir) {
    if ((Test-Path $Dir) -and -not (($env:Path -split ';') -contains $Dir)) {
        $env:Path = "$Dir;$env:Path"
    }
}

function Test-Command([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Find-VsCppTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $null }
    $path = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($path) { return $path }
    return $null
}

# Where the common installers put things that are not always on PATH.
function Add-KnownToolDirs {
    Add-ToPathIfPresent (Join-Path $env:USERPROFILE ".cargo\bin")
    Add-ToPathIfPresent (Join-Path $env:ProgramFiles "NASM")
    Add-ToPathIfPresent (Join-Path $env:ProgramFiles "CMake\bin")
    Add-ToPathIfPresent (Join-Path $env:ProgramFiles "LLVM\bin")
    Add-ToPathIfPresent "C:\Strawberry\perl\bin"
    Add-ToPathIfPresent "C:\Strawberry\c\bin"
}

# name, winget id, test
$prerequisites = @(
    @{ Name = "Visual Studio Build Tools (C++)"; Id = "Microsoft.VisualStudio.2022.BuildTools";
       Args = @("--override", "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended");
       Test = { [bool](Find-VsCppTools) } },
    @{ Name = "Rust (rustup)"; Id = "Rustlang.Rustup"; Args = @(); Test = { Test-Command "cargo" } },
    @{ Name = "Node.js LTS"; Id = "OpenJS.NodeJS.LTS"; Args = @(); Test = { Test-Command "npm" } },
    @{ Name = "Git for Windows"; Id = "Git.Git"; Args = @(); Test = { Test-Command "git" } },
    @{ Name = "CMake"; Id = "Kitware.CMake"; Args = @(); Test = { Test-Command "cmake" } },
    @{ Name = "LLVM (libclang)"; Id = "LLVM.LLVM"; Args = @();
       Test = { Test-Path (Join-Path $env:ProgramFiles "LLVM\bin\libclang.dll") } },
    # OpenSSL's Configure rejects MSYS/Git perl, so this must be Strawberry's.
    @{ Name = "Strawberry Perl"; Id = "StrawberryPerl.StrawberryPerl"; Args = @();
       Test = { (Test-Command "perl") -and ((& perl -e 'print $^O') -eq "MSWin32") } },
    @{ Name = "NASM"; Id = "NASM.NASM"; Args = @(); Test = { Test-Command "nasm" } }
)

Write-Step "Checking build tools"
Add-KnownToolDirs
$missing = @()
foreach ($p in $prerequisites) {
    if (& $p.Test) { Write-Ok $p.Name } else { Write-Missing $p.Name; $missing += $p }
}

if ($missing.Count -gt 0) {
    if (-not $InstallPrerequisites) {
        Write-Host "`nMissing tools. Re-run with -InstallPrerequisites to install them with winget." -ForegroundColor Yellow
        exit 1
    }
    if (-not (Test-Command "winget")) {
        throw "winget is not available. Install 'App Installer' from the Microsoft Store, then re-run."
    }
    foreach ($p in $missing) {
        Write-Step "Installing $($p.Name)"
        $wingetArgs = @("install", "--id", $p.Id, "--exact", "--accept-package-agreements", "--accept-source-agreements") + $p.Args
        & winget @wingetArgs
        # winget returns non-zero for "already installed" too; the re-check below is the judge.
    }
    Update-SessionPath
    Add-KnownToolDirs
    Write-Step "Re-checking build tools"
    $still = @()
    foreach ($p in $prerequisites) {
        if (& $p.Test) { Write-Ok $p.Name } else { Write-Missing $p.Name; $still += $p.Name }
    }
    if ($still.Count -gt 0) {
        throw "Still missing after install: $($still -join ', '). Open a new PowerShell window and re-run; if that does not help, install them by hand."
    }
}

# bindgen (whisper.cpp's bindings) looks for libclang here.
if (-not $env:LIBCLANG_PATH) {
    $env:LIBCLANG_PATH = Join-Path $env:ProgramFiles "LLVM\bin"
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$desktopDir = Join-Path $repoRoot "desktop"

Write-Step "Rust toolchain"
Push-Location $repoRoot
try {
    # rust-toolchain.toml pins the version; this makes rustup fetch it now
    # rather than halfway through the build.
    & rustup show active-toolchain
    if ($LASTEXITCODE -ne 0) { throw "rustup could not install the pinned toolchain" }
} finally { Pop-Location }

Write-Step "Installing desktop dependencies"
Push-Location $desktopDir
try {
    & npm ci
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed" }

    Write-Step "Building the installer (the first build takes a long while)"
    # tauri's beforeBuildCommand builds the dashboard, the gateway sidecar and
    # the Whisper sidecar before bundling.
    & npm run build -- --bundles $Bundles
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
} finally { Pop-Location }

$bundleDir = Join-Path $desktopDir "src-tauri\target\release\bundle"
$installer = Get-ChildItem -Path $bundleDir -Recurse -Include "*-setup.exe", "*.msi" -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $installer) { throw "The build finished but no installer was found under $bundleDir" }

Write-Step "Done"
Write-Host "Installer: $($installer.FullName)" -ForegroundColor Green
if (-not (Test-Command "bash") -and -not (Test-Path (Join-Path $env:ProgramFiles "Git\bin\bash.exe"))) {
    Write-Host "Note: the Claude CLI needs Git for Windows (bash) at runtime." -ForegroundColor Yellow
}
