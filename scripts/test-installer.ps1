<#
.SYNOPSIS
    End-to-end test of the installer built by package.ps1.

.DESCRIPTION
    Installs silently into a temporary folder, checks what was installed
    (files, registry, browser registration, signatures if the build is
    signed), exercises the installed application (start hidden in the tray,
    bring the window back from a second instance, refuse callers without the
    secret, ask it to quit), then uninstalls and checks that nothing is left
    behind. Uses its own data folder, so an existing RDM profile is not touched.

    If RDM is really installed on this computer, its registry entries and
    shortcuts (which a second install and uninstall would overwrite and then
    remove) are saved first and put back at the end. A copy of RDM that is
    running is left alone: the test copy uses its own port and is told apart
    by its process.
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$setup = Get-ChildItem (Join-Path $root "dist") -Filter "RDM-*-Setup.exe" | Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $setup) { throw "Run scripts\package.ps1 first." }

$work = Join-Path $env:TEMP ("rdm-install-test-" + (Get-Random))
$dir = Join-Path $work "app"
$data = Join-Path $work "data"
$saved = Join-Path $work "saved"
New-Item -ItemType Directory -Force $work, $saved | Out-Null
$others = @(Get-Process rdm -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
# Every RDM process started from here (also by the installer) uses the test profile,
# on a port of its own.
$env:RDM_DATA_DIR = $data
$apiPort = 47952
New-Item -ItemType Directory -Force $data | Out-Null
[IO.File]::WriteAllText((Join-Path $data "config.json"), "{ `"api_port`": $apiPort }", (New-Object Text.UTF8Encoding($false)))

# Windows are looked up per process, so a copy of RDM the user has open is never mistaken for the test copy.
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
namespace RdmTest {
    public static class Win {
        delegate bool EnumProc(IntPtr hwnd, IntPtr lparam);
        [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr lparam);
        [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int max);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder text, int max);
        [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);

        // The top-level window of process `pid` with this title or (if title is null) this class.
        public static IntPtr Find(int pid, string title, string cls) {
            IntPtr found = IntPtr.Zero;
            EnumWindows((hwnd, l) => {
                uint owner;
                GetWindowThreadProcessId(hwnd, out owner);
                if (owner != (uint)pid) return true;
                var text = new StringBuilder(256);
                if (title != null) GetWindowText(hwnd, text, 256); else GetClassName(hwnd, text, 256);
                if (text.ToString() != (title ?? cls)) return true;
                found = hwnd;
                return false;
            }, IntPtr.Zero);
            return found;
        }
    }
}
"@
function Find-Rdm($Process) { [RdmTest.Win]::Find($Process.Id, "Rapid Download Manager", $null) }
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

$hostName = "io.github.boulbeba115.rdm"
$uninstKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\RapidDownloadManager"
$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$startMenu = Join-Path ([Environment]::GetFolderPath("Programs")) "Rapid Download Manager"
$desktopLink = Join-Path ([Environment]::GetFolderPath("Desktop")) "Rapid Download Manager.lnk"
$browserKeys = "Google\Chrome", "Microsoft\Edge", "Chromium", "BraveSoftware\Brave-Browser" |
    ForEach-Object { "Software\$_\NativeMessagingHosts\$hostName" }

# ---- Save what a real installation on this computer has, to put it back afterwards.
$savedKeys = @("Software\RapidDownloadManager", "Software\Microsoft\Windows\CurrentVersion\Uninstall\RapidDownloadManager") + $browserKeys
function Invoke-Reg([string]$Arguments) { cmd /c "reg $Arguments >nul 2>&1"; return $LASTEXITCODE -eq 0 }
$i = 0
$keyFiles = @{}
foreach ($key in $savedKeys) {
    $i++
    $file = Join-Path $saved "key$i.reg"
    if (Invoke-Reg "export `"HKCU\$key`" `"$file`" /y") { $keyFiles[$key] = $file }
}
$savedRun = (Get-ItemProperty $runKey -ErrorAction SilentlyContinue).RapidDownloadManager
if (Test-Path $startMenu) { Copy-Item $startMenu (Join-Path $saved "startmenu") -Recurse }
if (Test-Path $desktopLink) { Copy-Item $desktopLink (Join-Path $saved "desktop.lnk") }
if ($keyFiles.Count -gt 0) { Write-Host "An existing installation was found; its registry entries and shortcuts will be restored afterwards." }

function Restore-Existing {
    foreach ($key in $savedKeys) {
        Invoke-Reg "delete `"HKCU\$key`" /f" | Out-Null
        if ($keyFiles.ContainsKey($key)) { Invoke-Reg "import `"$($keyFiles[$key])`"" | Out-Null }
    }
    if ($savedRun) { Set-ItemProperty $runKey -Name RapidDownloadManager -Value $savedRun }
    if (Test-Path (Join-Path $saved "startmenu")) {
        Remove-Item $startMenu -Recurse -Force -ErrorAction SilentlyContinue
        Copy-Item (Join-Path $saved "startmenu") $startMenu -Recurse
    }
    if (Test-Path (Join-Path $saved "desktop.lnk")) { Copy-Item (Join-Path $saved "desktop.lnk") $desktopLink -Force }
}

try {
    Write-Host "Installing $($setup.Name) into $dir"
    # /D must be last and unquoted (NSIS rule).
    $p = Start-Process $setup.FullName -ArgumentList "/S", "/D=$dir" -PassThru
    Check "installer exits" ($p.WaitForExit(120000))
    Check "installer exit code" $p.ExitCode 0

    $files = "rdm.exe", "rdmc.exe", "rdm-host.exe", "uninstall.exe", "LICENSE.txt", "README.md", "extension\manifest.json",
        "extension\background.js", "extension\content.js", "extension\page-hook.js", "extension\popup.html", "extension\popup.css", "extension\popup.js", "extension\icons\128.png"
    foreach ($f in $files) { Check "installed $f" (Test-Path (Join-Path $dir $f)) }
    $reg = Get-ItemProperty $uninstKey -ErrorAction SilentlyContinue
    Check "Apps & features entry" ($null -ne $reg)
    Check "  version" $reg.DisplayVersion ($setup.Name -replace '^RDM-(.+)-Setup\.exe$', '$1')
    Check "  install location" $reg.InstallLocation $dir
    Check "  size recorded" ($reg.EstimatedSize -gt 5000)
    Check "Start menu shortcut" (Test-Path (Join-Path $startMenu "Rapid Download Manager.lnk"))
    Check "desktop shortcut" (Test-Path $desktopLink)
    # Not chosen in a silent install: whatever was there before (nothing, or a real installation's entry) stays.
    Check "autostart is not touched unless chosen" (Get-ItemProperty $runKey -ErrorAction SilentlyContinue).RapidDownloadManager $savedRun
    $ver = (Get-Item (Join-Path $dir "rdm.exe")).VersionInfo
    Check "exe carries product name" $ver.ProductName "Rapid Download Manager"
    Check "rdmc runs" ((& (Join-Path $dir "rdmc.exe") --help | Select-Object -First 1) -match "rdmc")

    Write-Host "Browser registration"
    $manifestPath = Join-Path $dir "rdm-host.json"
    Check "host manifest written" (Test-Path $manifestPath)
    foreach ($key in $browserKeys) {
        Check "registered under $key" (Get-ItemProperty "HKCU:\$key" -ErrorAction SilentlyContinue).'(default)' $manifestPath
    }
    $manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
    $extension = Get-Content (Join-Path $dir "extension\manifest.json") -Raw | ConvertFrom-Json
    Check "manifest names the host" $manifest.name $hostName
    Check "manifest points at the installed host" $manifest.path (Join-Path $dir "rdm-host.exe")
    Check "only RDM's own extension is allowed" (@($manifest.allowed_origins) -join ",") "chrome-extension://pfdfjjjecmhdcmoedibbhgajnogkiojg/"
    Check "the extension asks for no access to the local port" (@($extension.host_permissions) -join ",") "http://*/*,https://*/*"
    Check "the extension version matches the application" $extension.version $reg.DisplayVersion

    if ((Get-AuthenticodeSignature $setup.FullName).SignerCertificate) {
        Write-Host "Signatures (this build is signed)"
        $signer = (Get-AuthenticodeSignature $setup.FullName).SignerCertificate.Thumbprint
        foreach ($f in "rdm.exe", "rdmc.exe", "rdm-host.exe", "uninstall.exe") {
            $sig = Get-AuthenticodeSignature (Join-Path $dir $f)
            Check "$f is signed by the same certificate" $sig.SignerCertificate.Thumbprint $signer
            Check "$f is intact and time-stamped" ($sig.Status -in "Valid", "UnknownError" -and $null -ne $sig.TimeStamperCertificate)
        }
    }
    else { Write-Host "Signatures: skipped (unsigned build)" }

    Write-Host "Running the installed application"
    $exe = Join-Path $dir "rdm.exe"
    $app = Start-Process $exe -ArgumentList "--minimized" -PassThru
    Check "window is created" (Wait-Until { (Find-Rdm $app) -ne [IntPtr]::Zero })
    Start-Sleep -Seconds 2
    Check "starts hidden in the tray with --minimized" ([RdmTest.Win]::IsWindowVisible((Find-Rdm $app))) $false
    Check "still running while hidden" (-not $app.HasExited)
    Check "tray icon window exists" ([RdmTest.Win]::Find($app.Id, $null, "tray_icon_app") -ne [IntPtr]::Zero)

    # The local port answers nobody who does not hold this run's secret.
    function Get-Status([hashtable]$Headers) {
        try { return [int](Invoke-WebRequest "http://127.0.0.1:$apiPort/config" -Headers $Headers -UseBasicParsing -TimeoutSec 5).StatusCode }
        catch { if ($_.Exception.Response) { return [int]$_.Exception.Response.StatusCode } else { return -1 } }
    }
    $token = (Get-Content (Join-Path $data "api-token") -Raw -ErrorAction SilentlyContinue)
    Check "a secret was created for this run" ("$token".Trim().Length) 64
    Check "the local port refuses a caller without the secret" (Get-Status @{}) 403
    Check "  and one with the old 'native client' header" (Get-Status @{ "X-RDM-Client" = "cli" }) 403
    Check "  and anything coming from a browser, even with the secret" (Get-Status @{ "X-RDM-Token" = "$token".Trim(); "Origin" = "chrome-extension://pfdfjjjecmhdcmoedibbhgajnogkiojg" }) 403
    Check "  but accepts RDM's own programs" (Get-Status @{ "X-RDM-Token" = "$token".Trim() }) 200

    $second = Start-Process $exe -PassThru
    Check "second instance hands over and exits" ($second.WaitForExit(8000))
    Check "window comes back when started again" (Wait-Until { [RdmTest.Win]::IsWindowVisible((Find-Rdm $app)) } 8)

    Start-Process $exe -ArgumentList "/quit" -Wait
    Check "/quit closes the running instance" ($app.WaitForExit(10000))
    Check "log file written" (Test-Path (Join-Path $data "rdm.log"))

    Write-Host "Uninstalling (with the application running)"
    $app = Start-Process $exe -ArgumentList "--minimized" -PassThru
    Wait-Until { (Find-Rdm $app) -ne [IntPtr]::Zero } | Out-Null
    Start-Sleep -Seconds 2
    # _?= makes the uninstaller run in place so we can wait for it.
    $u = Start-Process (Join-Path $dir "uninstall.exe") -ArgumentList "/S", "_?=$dir" -PassThru
    Check "uninstaller exits" ($u.WaitForExit(120000))
    Check "running application was closed" ($app.WaitForExit(5000))
    foreach ($f in "rdm.exe", "rdmc.exe", "rdm-host.exe", "rdm-host.json", "extension") { Check "removed $f" (Test-Path (Join-Path $dir $f)) $false }
    Check "Apps & features entry removed" (Test-Path $uninstKey) $false
    foreach ($key in $browserKeys) { Check "browser registration removed ($key)" (Test-Path "HKCU:\$key") $false }
    Check "Start menu folder removed" (Test-Path $startMenu) $false
    Check "desktop shortcut removed" (Test-Path $desktopLink) $false
    $gone = @($others | Where-Object { -not (Get-Process -Id $_ -ErrorAction SilentlyContinue) })
    Check "a copy of RDM that was already running was left alone" $gone.Count 0
}
finally {
    Get-Process rdm -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$work*" } | Stop-Process -Force -ErrorAction SilentlyContinue
    Restore-Existing
    # With _?= the uninstaller cannot delete itself; tidy up what the test created.
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
Write-Host "passed $script:pass, failed $script:fail"
if ($script:fail -gt 0) { exit 1 }
