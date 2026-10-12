<#
.SYNOPSIS
    Build the release artifacts: the installer and the portable zip.

.DESCRIPTION
    1. Builds the release binaries.
    2. Stamps the icon and version information into rdm.exe and rdmc.exe.
    3. Builds dist\RDM-<version>-Setup.exe with NSIS.
    4. Builds dist\RDM-<version>-portable-win64.zip (settings stay next to the exe).
    5. Writes dist\SHA256SUMS.txt.

    NSIS is looked up on PATH, then in tools\nsis-*\ (see README, "Packaging").

.EXAMPLE
    .\scripts\package.ps1
    .\scripts\package.ps1 -SkipBuild
#>
param([switch]$SkipBuild)

$ErrorActionPreference = "Stop"

# Run a native program, show its output, and fail on a non-zero exit code.
# (Windows PowerShell turns anything a program writes to stderr into an error
# when ErrorActionPreference is Stop, and cargo reports progress on stderr.)
function Invoke-Tool {
    param([string]$Exe, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $Exe @Arguments 2>&1 | ForEach-Object { Write-Host "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($code -ne 0) { throw "$Exe failed with exit code $code" }
}

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path $cargoBin) { $env:Path = "$cargoBin;$env:Path" }

# The workspace version is the single source of truth.
$cargoToml = Join-Path $root "Cargo.toml"
$version = (Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
Write-Host "Packaging Rapid Download Manager $version"

if (-not $SkipBuild) {
    Invoke-Tool cargo @("build", "--release", "-p", "rdm-app", "-p", "rdm-cli", "-p", "xtask")
}

$release = Join-Path $root "target\release"
$dist = Join-Path $root "dist"
$stage = Join-Path $dist "stage"
$tmp = Join-Path $dist "tmp"
foreach ($dir in $stage, $tmp) {
    if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    New-Item -ItemType Directory -Force $dir | Out-Null
}

# ---- Stage the files that ship, and stamp the executables.
$xtask = Join-Path $release "xtask.exe"
$icon = Join-Path $tmp "rdm.ico"
Copy-Item (Join-Path $release "rdm.exe"), (Join-Path $release "rdmc.exe") $stage
Invoke-Tool $xtask @("stamp", (Join-Path $stage "rdm.exe"), "Rapid Download Manager")
Invoke-Tool $xtask @("stamp", (Join-Path $stage "rdmc.exe"), "Rapid Download Manager command-line tool")
Invoke-Tool $xtask @("icon", $icon)

Copy-Item (Join-Path $root "LICENSE") (Join-Path $stage "LICENSE.txt")
Copy-Item (Join-Path $root "README.md") $stage
$stageExt = Join-Path $stage "extension"
New-Item -ItemType Directory -Force $stageExt | Out-Null
Copy-Item (Join-Path $root "extension\*") $stageExt -Recurse

# ---- Installer.
$makensis = (Get-Command makensis -ErrorAction SilentlyContinue).Source
if (-not $makensis) {
    $makensis = Get-ChildItem (Join-Path $root "tools") -Recurse -Filter makensis.exe -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}
if (-not $makensis) { throw "NSIS (makensis.exe) not found on PATH or under tools. See README, 'Packaging'." }

$setup = Join-Path $dist "RDM-$version-Setup.exe"
$nsi = Join-Path $root "installer\rdm.nsi"
Invoke-Tool $makensis @("/V2", "/DVERSION=$version", "/DSTAGE=$stage", "/DICON=$icon", "/DOUTFILE=$setup", $nsi)

# ---- Portable zip: same files plus the marker that keeps settings beside the exe.
$portableName = "RDM-$version-portable-win64"
$portableDir = Join-Path $tmp $portableName
Copy-Item $stage $portableDir -Recurse
Set-Content -Path (Join-Path $portableDir "portable.txt") -Encoding ascii -Value @(
    "This file makes Rapid Download Manager portable:"
    "settings and the download list are kept in the 'data' folder next to rdm.exe"
    "instead of %APPDATA%\RDM. Delete this file to use the normal location."
)
$zip = Join-Path $dist "$portableName.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $portableDir -DestinationPath $zip -CompressionLevel Optimal

# ---- Checksums.
$sums = foreach ($file in $setup, $zip) {
    "{0}  {1}" -f (Get-FileHash $file -Algorithm SHA256).Hash.ToLower(), (Split-Path -Leaf $file)
}
Set-Content -Path (Join-Path $dist "SHA256SUMS.txt") -Encoding ascii -Value $sums

Write-Host ""
Write-Host "Artifacts in $dist"
foreach ($file in (Get-Item $setup, $zip)) {
    Write-Host ("  {0,-40} {1,7:N1} MB" -f $file.Name, ($file.Length / 1MB))
}
foreach ($line in $sums) { Write-Host "  $line" }
