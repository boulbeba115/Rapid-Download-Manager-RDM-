<#
.SYNOPSIS
    End-to-end test of the installer built by package.ps1.

.DESCRIPTION
    Installs silently into a temporary folder, checks what was installed,
    exercises the installed application (start hidden in the tray, bring the
    window back from a second instance, ask it to quit), then uninstalls and
    checks that nothing is left behind. Uses its own data folder, so an
    existing RDM profile is not touched.
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$setup = Get-ChildItem (Join-Path $root "dist") -Filter "RDM-*-Setup.exe" | Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $setup) { throw "Run scripts\package.ps1 first." }

$work = Join-Path $env:TEMP ("rdm-install-test-" + (Get-Random))
$dir = Join-Path $work "app"
$data = Join-Path $work "data"
New-Item -ItemType Directory -Force $work | Out-Null
# Every RDM process started from here (also by the installer) uses the test profile.
$env:RDM_DATA_DIR = $data

Add-Type -Namespace RdmTest -Name Win -MemberDefinition @"
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindow(System.IntPtr cls, string title);
[DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern System.IntPtr FindWindowClass(string cls, System.IntPtr title);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr hwnd);
"@
function Find-Rdm { [RdmTest.Win]::FindWindow([IntPtr]::Zero, "Rapid Download Manager") }
function Wait-Until([scriptblock]$Condition, [int]$Seconds = 15) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) { if (& $Condition) { return $true }; Start-Sleep -Milliseconds 200 }
    return $false
}

$script:pass = 0; $script:fail = 0
function Check([string]$Name, $Got, $Want = $true) {
    if ("$Got" -eq "$Want") { $script:pass++; Write-Host "  ok    $Name" }
    else { $script:fail++; Write-Host "  FAIL  $Name  (got '$Got', wanted '$Want')" }
}

$uninstKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\RapidDownloadManager"
$startMenu = Join-Path ([Environment]::GetFolderPath("Programs")) "Rapid Download Manager"
$desktopLink = Join-Path ([Environment]::GetFolderPath("Desktop")) "Rapid Download Manager.lnk"

Write-Host "Installing $($setup.Name) into $dir"
# /D must be last and unquoted (NSIS rule).
$p = Start-Process $setup.FullName -ArgumentList "/S", "/D=$dir" -PassThru
Check "installer exits" ($p.WaitForExit(120000))
Check "installer exit code" $p.ExitCode 0

foreach ($f in "rdm.exe", "rdmc.exe", "uninstall.exe", "LICENSE.txt", "README.md", "extension\manifest.json", "extension\background.js") {
    Check "installed $f" (Test-Path (Join-Path $dir $f))
}
$reg = Get-ItemProperty $uninstKey -ErrorAction SilentlyContinue
Check "Apps & features entry" ($null -ne $reg)
Check "  version" $reg.DisplayVersion ($setup.Name -replace '^RDM-(.+)-Setup\.exe$', '$1')
Check "  install location" $reg.InstallLocation $dir
Check "  size recorded" ($reg.EstimatedSize -gt 5000)
Check "Start menu shortcut" (Test-Path (Join-Path $startMenu "Rapid Download Manager.lnk"))
Check "desktop shortcut" (Test-Path $desktopLink)
Check "no autostart unless chosen" ($null -eq (Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -ErrorAction SilentlyContinue).RapidDownloadManager)
$ver = (Get-Item (Join-Path $dir "rdm.exe")).VersionInfo
Check "exe carries product name" $ver.ProductName "Rapid Download Manager"
Check "rdmc runs" ((& (Join-Path $dir "rdmc.exe") --help | Select-Object -First 1) -match "rdmc")

Write-Host "Running the installed application"
$exe = Join-Path $dir "rdm.exe"
$app = Start-Process $exe -ArgumentList "--minimized" -PassThru
Check "window is created" (Wait-Until { (Find-Rdm) -ne [IntPtr]::Zero })
Start-Sleep -Seconds 2
Check "starts hidden in the tray with --minimized" ([RdmTest.Win]::IsWindowVisible((Find-Rdm))) $false
Check "still running while hidden" (-not $app.HasExited)
Check "tray icon window exists" ([RdmTest.Win]::FindWindowClass("tray_icon_app", [IntPtr]::Zero) -ne [IntPtr]::Zero)

$second = Start-Process $exe -PassThru
Check "second instance hands over and exits" ($second.WaitForExit(8000))
Check "window comes back when started again" (Wait-Until { [RdmTest.Win]::IsWindowVisible((Find-Rdm)) } 8)

Start-Process $exe -ArgumentList "/quit" -Wait
Check "/quit closes the running instance" ($app.WaitForExit(10000))
Check "log file written" (Test-Path (Join-Path $data "rdm.log"))

Write-Host "Uninstalling (with the application running)"
$app = Start-Process $exe -ArgumentList "--minimized" -PassThru
Wait-Until { (Find-Rdm) -ne [IntPtr]::Zero } | Out-Null
Start-Sleep -Seconds 2
# _?= makes the uninstaller run in place so we can wait for it.
$u = Start-Process (Join-Path $dir "uninstall.exe") -ArgumentList "/S", "_?=$dir" -PassThru
Check "uninstaller exits" ($u.WaitForExit(120000))
Check "running application was closed" ($app.WaitForExit(5000))
foreach ($f in "rdm.exe", "rdmc.exe", "extension") { Check "removed $f" (Test-Path (Join-Path $dir $f)) $false }
Check "Apps & features entry removed" (Test-Path $uninstKey) $false
Check "Start menu folder removed" (Test-Path $startMenu) $false
Check "desktop shortcut removed" (Test-Path $desktopLink) $false

# With _?= the uninstaller cannot delete itself; tidy up what the test created.
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
Write-Host "passed $script:pass, failed $script:fail"
if ($script:fail -gt 0) { exit 1 }
