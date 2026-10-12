<#
.SYNOPSIS
    End-to-end checks that need a real desktop: clipboard monitoring and the
    browser extension running in Microsoft Edge.

.DESCRIPTION
    Uses the packaged application in dist\stage (run scripts\package.ps1
    first), a throwaway RDM profile and a throwaway Edge profile. The clipboard
    text is saved and put back. An Edge window opens for a few seconds.

    What RDM did is read from its log file (rdm.log).
#>
param([switch]$SkipBrowser)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "dist\stage\rdm.exe"
$rdmc = Join-Path $root "dist\stage\rdmc.exe"
if (-not (Test-Path $exe)) { throw "Run scripts\package.ps1 first." }

$work = Join-Path $env:TEMP ("rdm-integration-" + (Get-Random))
$data = Join-Path $work "data"
New-Item -ItemType Directory -Force $data | Out-Null
$env:RDM_DATA_DIR = $data
$log = Join-Path $data "rdm.log"

$script:pass = 0; $script:fail = 0
function Check([string]$Name, $Got, $Want = $true) {
    if ("$Got" -eq "$Want") { $script:pass++; Write-Host "  ok    $Name" }
    else { $script:fail++; Write-Host "  FAIL  $Name  (got '$Got', wanted '$Want')" }
}
function Wait-Log([string]$Text, [int]$Seconds = 20) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $log) -and (Select-String -Path $log -SimpleMatch $Text -Quiet)) { return $true }
        Start-Sleep -Milliseconds 300
    }
    return $false
}

# Clipboard monitoring on; everything else default (so the extension's default port applies).
$config = '{ "monitor_clipboard": true, "net": { "proxy": { "mode": "none" } } }'
[IO.File]::WriteAllText((Join-Path $data "config.json"), $config, (New-Object Text.UTF8Encoding($false)))

$server = Start-Process $rdmc -ArgumentList "serve", "-s", "5M", "-p", "18081" -WindowStyle Hidden -PassThru
$app = Start-Process $exe -PassThru
Check "application started" (Wait-Log "starting" 15)
Start-Sleep -Seconds 2

Write-Host "Clipboard monitoring"
Add-Type -AssemblyName System.Windows.Forms
$saved = [System.Windows.Forms.Clipboard]::GetText()
try {
    [System.Windows.Forms.Clipboard]::SetText("just some ordinary text")
    Start-Sleep -Seconds 2
    [System.Windows.Forms.Clipboard]::SetText("http://127.0.0.1:18081/page.html")
    Start-Sleep -Seconds 2
    Check "ordinary text and non-file links are ignored" ([bool](Select-String -Path $log -SimpleMatch "on the clipboard" -Quiet)) $false
    [System.Windows.Forms.Clipboard]::SetText("http://127.0.0.1:18081/clip-test.zip")
    Check "a download link is picked up" (Wait-Log "download link on the clipboard: http://127.0.0.1:18081/clip-test.zip" 10)
}
finally {
    if ($saved) { [System.Windows.Forms.Clipboard]::SetText($saved) } else { [System.Windows.Forms.Clipboard]::Clear() }
}

if (-not $SkipBrowser) {
    Write-Host "Browser extension in Microsoft Edge"
    $edge = "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe"
    if (-not (Test-Path $edge)) { $edge = "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe" }
    if (-not (Test-Path $edge)) {
        Write-Host "  skipped: Microsoft Edge not found"
    }
    else {
        $profile = Join-Path $work "edge"
        $downloads = Join-Path $work "browser-downloads"
        New-Item -ItemType Directory -Force (Join-Path $profile "Default"), $downloads | Out-Null
        # Keep the browser's own download (if the extension does not take it) out of the real Downloads folder.
        $prefs = @{ download = @{ default_directory = $downloads; prompt_for_download = $false } } | ConvertTo-Json -Depth 4
        [IO.File]::WriteAllText((Join-Path $profile "Default\Preferences"), $prefs, (New-Object Text.UTF8Encoding($false)))

        $common = @(
            "--user-data-dir=$profile", "--no-first-run", "--no-default-browser-check", "--disable-sync",
            "--disable-features=DisableLoadExtensionCommandLineSwitch",
            "--load-extension=$(Join-Path $root 'extension')"
        )
        # First start loads the extension; the download is requested once its worker is up.
        Start-Process $edge -ArgumentList ($common + "about:blank") | Out-Null
        Start-Sleep -Seconds 7
        Start-Process $edge -ArgumentList ($common + "http://127.0.0.1:18081/from-browser.bin") | Out-Null

        $taken = Wait-Log "add request from the browser extension: http://127.0.0.1:18081/from-browser.bin" 25
        Check "the extension hands the download to RDM" $taken
        Start-Sleep -Seconds 2
        $kept = @(Get-ChildItem $downloads -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "from-browser*" -and $_.Extension -ne ".crdownload" })
        Check "the browser's own copy was cancelled" $kept.Count 0

        Get-CimInstance Win32_Process -Filter "Name = 'msedge.exe'" |
            Where-Object { $_.CommandLine -like "*$profile*" } |
            ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    }
}

Start-Process $exe -ArgumentList "/quit" -Wait
Check "application quits" ($app.WaitForExit(10000))
if (-not $app.HasExited) { $app.Kill() }
if (-not $server.HasExited) { $server.Kill() }

Write-Host "--- rdm.log ---"
Get-Content $log | ForEach-Object { Write-Host "  $_" }
Start-Sleep -Seconds 1
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
Write-Host "passed $script:pass, failed $script:fail"
if ($script:fail -gt 0) { exit 1 }
