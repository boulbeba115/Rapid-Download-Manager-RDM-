<#
.SYNOPSIS
    End-to-end checks that need a real desktop: clipboard monitoring and the
    browser extension running in Microsoft Edge.

.DESCRIPTION
    Uses the packaged application in dist\stage (run scripts\package.ps1
    first), a throwaway RDM profile on its own port and a throwaway Edge
    profile. The clipboard text is saved and put back. An Edge window opens
    for a little while.

    The browser part is the real path, end to end: a page in Edge starts a
    download or shows a video, the extension reacts, Edge starts rdm-host.exe
    through native messaging, and the host hands the link to the application.
    For that the host in dist\stage is registered for the current user while
    the test runs; whatever was registered before is put back afterwards. A
    copy of RDM that is already running is left alone (the test copy has its
    own port and profile), but with clipboard monitoring on it will notice
    the test's link too.

    What RDM did is read from its log file (rdm.log).
#>
param([switch]$SkipBrowser)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$stage = Join-Path $root "dist\stage"
$exe = Join-Path $stage "rdm.exe"
$rdmc = Join-Path $stage "rdmc.exe"
$rdmHost = Join-Path $stage "rdm-host.exe"
if (-not (Test-Path $exe)) { throw "Run scripts\package.ps1 first." }

$work = Join-Path $env:TEMP ("rdm-integration-" + (Get-Random))
$data = Join-Path $work "data"
New-Item -ItemType Directory -Force $data | Out-Null
$env:RDM_DATA_DIR = $data
$log = Join-Path $data "rdm.log"
# Not the default port: stays clear of a real installation, and shows that the
# browser host finds the application through its settings.
$apiPort = 47953
$files = "http://127.0.0.1:18081"

$script:pass = 0; $script:fail = 0
function Check([string]$Name, $Got, $Want = $true) {
    if ("$Got" -eq "$Want") { $script:pass++; Write-Host "  ok    $Name" }
    else { $script:fail++; Write-Host "  FAIL  $Name  (got '$Got', wanted '$Want')" }
}
function Test-Log([string]$Text) { (Test-Path $log) -and (Select-String -Path $log -SimpleMatch $Text -Quiet) }
function Wait-Log([string]$Text, [int]$Seconds = 20) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (Test-Log $Text) { return $true }
        Start-Sleep -Milliseconds 300
    }
    return $false
}
function Stop-TestApp {
    Get-Process rdm -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$stage*" } | ForEach-Object {
        Start-Process $exe -ArgumentList "/quit" -Wait
        if (-not $_.WaitForExit(10000)) { $_.Kill() }
    }
}

# ---- Chrome DevTools protocol, just enough to drive a tab like a user would.
$none = [Threading.CancellationToken]::None
$script:cdpId = 0
function Open-Cdp([string]$Url) {
    $ws = New-Object System.Net.WebSockets.ClientWebSocket
    if (-not $ws.ConnectAsync([uri]$Url, $none).Wait(10000)) { throw "cannot connect to $Url" }
    return $ws
}
function Invoke-Cdp($Ws, [string]$Method, [hashtable]$Params = @{}) {
    $script:cdpId++
    $id = $script:cdpId
    $bytes = [Text.Encoding]::UTF8.GetBytes((@{ id = $id; method = $Method; params = $Params } | ConvertTo-Json -Depth 8 -Compress))
    $Ws.SendAsync((New-Object ArraySegment[byte] -ArgumentList @(, $bytes)), "Text", $true, $none).Wait(10000) | Out-Null
    $buffer = New-Object byte[] 65536
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        $stream = New-Object IO.MemoryStream
        do {
            $task = $Ws.ReceiveAsync((New-Object ArraySegment[byte] -ArgumentList @(, $buffer)), $none)
            if (-not $task.Wait(20000)) { throw "no answer to $Method" }
            $stream.Write($buffer, 0, $task.Result.Count)
        } until ($task.Result.EndOfMessage)
        $message = [Text.Encoding]::UTF8.GetString($stream.ToArray()) | ConvertFrom-Json
        # Events arrive on the same socket; only the reply to this call counts.
        if ($message.id -eq $id) { return $message }
    }
    throw "no answer to $Method"
}
# Run JavaScript in a tab or the extension's worker and return its (awaited) value.
function Invoke-Js($Ws, [string]$Expression, [switch]$UserGesture) {
    $reply = Invoke-Cdp $Ws "Runtime.evaluate" @{ expression = $Expression; awaitPromise = $true; returnByValue = $true; userGesture = [bool]$UserGesture }
    return $reply.result.result.value
}
function Send-Click($Page, [int]$X, [int]$Y) {
    foreach ($type in "mousePressed", "mouseReleased") {
        Invoke-Cdp $Page "Input.dispatchMouseEvent" @{ type = $type; x = $X; y = $Y; button = "left"; clickCount = 1 } | Out-Null
    }
}
# Put a download link on the page and click it with a real mouse click. (A click
# made by script is not a user action: the browser lets a page start one download
# that way and silently drops the rest.)
function Start-PageDownload($Page, [string]$Url, [switch]$NewTab) {
    $how = if ($NewTab) { "a.removeAttribute('download'); a.target = '_blank';" } else { "a.download = ''; a.target = '';" }
    $js = "(() => { let a = document.getElementById('rdm-test-link'); if (!a) { a = document.createElement('a'); a.id = 'rdm-test-link'; a.textContent = 'download'; " +
        "a.style.cssText = 'position:fixed;left:10px;top:400px;display:block;width:200px;height:40px;background:#ddd;z-index:99999'; document.body.appendChild(a); } " +
        "a.href = '$Url'; $how return 'ready'; })()"
    Invoke-Js $Page $js | Out-Null
    Send-Click $Page 60 420
}
# How many ordinary tabs the test browser has open.
function Get-TabCount {
    @(Invoke-RestMethod "http://127.0.0.1:$debugPort/json/list" -TimeoutSec 5 | Where-Object { $_.type -eq "page" -and $_.url -notlike "chrome-extension://*" -and $_.url -notlike "edge://*" }).Count
}

# Clipboard monitoring on; everything else default.
$config = '{ "monitor_clipboard": true, "api_port": ' + $apiPort + ', "net": { "proxy": { "mode": "none" } } }'
[IO.File]::WriteAllText((Join-Path $data "config.json"), $config, (New-Object Text.UTF8Encoding($false)))

$server = Start-Process $rdmc -ArgumentList "serve", "-s", "5M", "-p", "18081" -WindowStyle Hidden -PassThru
$app = Start-Process $exe -PassThru
Check "application started" (Wait-Log "starting" 15)
Start-Sleep -Seconds 2

# The browser registration of a real installation, to put back at the end.
$hostName = "io.github.boulbeba115.rdm"
$browserKeys = "Google\Chrome", "Microsoft\Edge", "Chromium", "BraveSoftware\Brave-Browser" |
    ForEach-Object { "HKCU:\Software\$_\NativeMessagingHosts\$hostName" }
$previous = @{}
foreach ($key in $browserKeys) { $previous[$key] = (Get-ItemProperty $key -ErrorAction SilentlyContinue).'(default)' }
$registered = $false

try {
    Write-Host "Clipboard monitoring"
    Add-Type -AssemblyName System.Windows.Forms
    $saved = [System.Windows.Forms.Clipboard]::GetText()
    try {
        [System.Windows.Forms.Clipboard]::SetText("just some ordinary text")
        Start-Sleep -Seconds 2
        [System.Windows.Forms.Clipboard]::SetText("$files/page.html")
        Start-Sleep -Seconds 2
        Check "ordinary text and non-file links are ignored" (Test-Log "on the clipboard") $false
        [System.Windows.Forms.Clipboard]::SetText("$files/clip-test.zip")
        Check "a download link is picked up" (Wait-Log "download link on the clipboard: $files/clip-test.zip" 10)
    }
    finally {
        if ($saved) { [System.Windows.Forms.Clipboard]::SetText($saved) } else { [System.Windows.Forms.Clipboard]::Clear() }
    }

    $edge = "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe"
    if (-not (Test-Path $edge)) { $edge = "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe" }
    if ($SkipBrowser) { Write-Host "Browser extension: skipped (-SkipBrowser)" }
    elseif (-not (Test-Path $edge)) { Write-Host "Browser extension: skipped (Microsoft Edge not found)" }
    else {
        Write-Host "Browser extension in Microsoft Edge"
        $registration = Start-Process $rdmHost -ArgumentList "--register" -Wait -PassThru
        $registered = $true
        Check "the browser host registers itself" $registration.ExitCode 0

        $profile = Join-Path $work "edge"
        $downloads = Join-Path $work "browser-downloads"
        New-Item -ItemType Directory -Force (Join-Path $profile "Default"), $downloads | Out-Null
        # Keep the browser's own downloads out of the real Downloads folder.
        $prefs = @{ download = @{ default_directory = $downloads; prompt_for_download = $false } } | ConvertTo-Json -Depth 4
        [IO.File]::WriteAllText((Join-Path $profile "Default\Preferences"), $prefs, (New-Object Text.UTF8Encoding($false)))

        $debugPort = 9333
        $flags = @(
            "--user-data-dir=$profile", "--no-first-run", "--no-default-browser-check", "--disable-sync",
            "--disable-features=DisableLoadExtensionCommandLineSwitch",
            "--load-extension=$(Join-Path $stage 'extension')",
            "--remote-debugging-port=$debugPort", "--window-size=1100,800", "about:blank"
        )
        Start-Process $edge -ArgumentList $flags | Out-Null

        # Wait until the extension's service worker and a tab are up.
        $worker = $null; $tab = $null
        $deadline = (Get-Date).AddSeconds(30)
        while ((-not $worker -or -not $tab) -and (Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 500
            try {
                $targets = Invoke-RestMethod "http://127.0.0.1:$debugPort/json/list" -TimeoutSec 3
                $worker = $targets | Where-Object { $_.url -like "chrome-extension://*/background.js" } | Select-Object -First 1
                $tab = $targets | Where-Object { $_.type -eq "page" -and $_.url -notlike "chrome-extension://*" } | Select-Object -First 1
            } catch { }
        }
        Check "Edge loaded the extension" ($null -ne $worker)
        Check "a tab is open" ($null -ne $tab)

        if ($worker -and $tab) {
            Check "the extension has its fixed identity" $worker.url "chrome-extension://pfdfjjjecmhdcmoedibbhgajnogkiojg/background.js"
            $sw = Open-Cdp $worker.webSocketDebuggerUrl
            $page = Open-Cdp $tab.webSocketDebuggerUrl
            Start-Sleep -Seconds 1

            $status = Invoke-Js $sw "chrome.runtime.sendNativeMessage('$hostName', { type: 'status' }).then((r) => JSON.stringify(r), (e) => 'ERROR ' + e.message)"
            Check "the extension reaches RDM through native messaging" ($status -like '*"running":true*') $true
            if ($status -notlike '*"running":true*') { Write-Host "        status: $status" }

            # A page with content, so links can be clicked on it.
            Invoke-Cdp $page "Page.navigate" @{ url = "$files/player.html" } | Out-Null
            Start-Sleep -Seconds 3

            Write-Host "  Downloads"
            Start-PageDownload $page "$files/from-browser.bin"
            Check "a download started by a page goes to RDM" (Wait-Log "add request from the browser extension: $files/from-browser.bin" 25)
            Start-Sleep -Seconds 2
            $kept = @(Get-ChildItem $downloads -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "from-browser*" })
            Check "the browser did not download it as well" $kept.Count 0
            $listed = Invoke-Js $sw "chrome.downloads.search({}).then((all) => all.filter((d) => d.url.includes('from-browser')).length)"
            Check "and it is gone from the browser's download list" $listed 0

            Start-PageDownload $page "$files/stays-in-browser.rdmtest"
            Start-Sleep -Seconds 4
            $listed = Invoke-Js $sw "chrome.downloads.search({}).then((all) => all.filter((d) => d.url.includes('stays-in-browser')).length)"
            Check "a file type RDM does not capture stays with the browser" $listed 1
            if ("$listed" -ne "1") {
                $all = Invoke-Js $sw "chrome.downloads.search({}).then((all) => JSON.stringify(all.map((d) => ({ url: d.url, state: d.state, danger: d.danger, error: d.error, paused: d.paused, file: d.filename, by: d.byExtensionName }))))"
                Write-Host "        the browser's downloads: $all"
                Write-Host "        files: $((Get-ChildItem $downloads -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }) -join ', ')"
            }
            Check "and is not offered to RDM" (Test-Log "stays-in-browser") $false

            Write-Host "  The download button on a video"
            Invoke-Cdp $page "Input.dispatchMouseEvent" @{ type = "mouseMoved"; x = 300; y = 170 } | Out-Null
            Start-Sleep -Milliseconds 400
            Invoke-Cdp $page "Input.dispatchMouseEvent" @{ type = "mouseMoved"; x = 320; y = 180 } | Out-Null
            Start-Sleep -Seconds 2
            $seen = Invoke-Js $sw "chrome.storage.session.get(null).then((all) => JSON.stringify(all))"
            Check "the video on the page was noticed" ($seen -like "*$files/movie.mp4*") $true
            # The test page grabs every click and keeps an invisible layer over its player, like
            # advert-laden video sites do (see PLAYER_PAGE in testserver.rs). The button is the
            # element the extension put in the browser's top layer.
            $find = "(() => { const h = document.querySelector('[popover]'); if (!h) return '{}'; const r = h.getBoundingClientRect(); " +
                "const top = document.elementFromPoint(r.left + 30, r.top + r.height / 2); " +
                "return JSON.stringify({ tag: h.tagName, open: h.shadowRoot !== null, shown: h.style.display, reachable: top === h, x: Math.round(r.left + 30), y: Math.round(r.top + r.height / 2), w: Math.round(r.width) }); })()"
            $button = Invoke-Js $page $find | ConvertFrom-Json
            Check "the button appears over the video" "$($button.tag) $($button.shown)" "DIV block"
            Check "the page cannot look inside the button" $button.open $false
            Check "the page's layer over the player does not cover the button" $button.reachable $true

            # A click the page makes up must do nothing...
            Invoke-Js $page "(() => { const h = document.querySelector('[popover]'); h.click(); h.dispatchEvent(new MouseEvent('click', { bubbles: true })); return 'done'; })()" | Out-Null
            Start-Sleep -Seconds 2
            Check "a click faked by the page is ignored" (Test-Log "movie.mp4") $false
            # ...a real one sends the video to RDM: the first click, and the page gets none of it.
            $tabsBefore = Get-TabCount
            Send-Click $page $button.x $button.y
            Check "the first real click sends the video to RDM" (Wait-Log "add request from the browser extension: $files/movie.mp4" 20)
            Start-Sleep -Seconds 1
            $hijack = Invoke-Js $page "JSON.stringify(window.hijack)" | ConvertFrom-Json
            Check "the page's click handlers never saw that click" "$($hijack.seen) seen, $($hijack.opened) opened" "0 seen, 0 opened"
            Check "and no new tab was opened" (Get-TabCount) $tabsBefore

            Write-Host "  Sites that feed their player from script (Facebook's data format, served locally)"
            # page-hook.js only runs on facebook.com; here it is put into the test page by hand.
            Invoke-Js $page "window.learned = []; addEventListener('message', (e) => { if (e.data && e.data.rdm === 'learned-media') learned.push(...e.data.items); }); 'listening'" | Out-Null
            Invoke-Cdp $page "Runtime.evaluate" @{ expression = [IO.File]::ReadAllText((Join-Path $stage "extension\page-hook.js")) } | Out-Null
            $load = "fetch('/fb-data.json?via=fetch').then((r) => r.json()).then((data) => new Promise((done) => { " +
                "const x = new XMLHttpRequest(); x.open('GET', '/fb-data.json?via=xhr'); x.onload = () => setTimeout(() => done(JSON.stringify({ intact: data.data.video.id === '1' && x.responseText.includes('next'), learned })), 400); x.send(); }))"
            $got = Invoke-Js $page $load | ConvertFrom-Json
            Check "the page still gets its own data untouched" $got.intact $true
            Check "addresses are read from data loaded with fetch and with XMLHttpRequest" (@($got.learned | ForEach-Object { "$($_.quality):" + ($_.url -replace '^.*/v/([a-z-]+)\.mp4.*$', '$1') }) -join " ") "hd:hd-fetch sd:sd-fetch sd:other-fetch hd:hd-xhr sd:sd-xhr sd:other-xhr"

            # What the worker makes of them, for a pretend tab: two videos, told apart by length.
            $items = $got.learned | ConvertTo-Json -Compress
            $labels = "(tab, length) => offersFor(tab, length).then((offers) => offers.map((o) => o.label.replace(/ \u00b7 /g, ' / ')).join(' | '))"
            $learn = "handlers.learn({ items: $items }, { tab: { id: 987654 } }).then(() => Promise.all([($labels)(987654, 136.4), ($labels)(987654, 137.9), ($labels)(987654, 500)])).then((r) => JSON.stringify(r))"
            $offered = Invoke-Js $sw $learn | ConvertFrom-Json
            Check "the video of that length is offered, in HD and SD" $offered[0] "HD video / 720p / 2:16 | SD video / 360p / 2:16"
            Check "another length selects the other video" $offered[1] "SD video / 360p / 2:17"
            Check "an unknown video is not answered with a guess from the page's data" $offered[2] ""
            $unsure = Invoke-Js $sw "handlers.media({ duration: 0 }, { tab: { id: 987654 } }).then((r) => JSON.stringify({ pending: r.pending === true, offers: r.media.length }))"
            Check "with several videos and no length yet, the button waits" $unsure '{"pending":true,"offers":0}'
            $forged = "handlers.learn({ items: [{ url: 'https://evil.example/v/x.mp4?efg=eyJ4cHZfYXNzZXRfaWQiOjQyNDI0MjQyNDIsImR1cmF0aW9uX3MiOjEzNn0', quality: 'hd' }, { url: 'javascript:alert(1)', quality: 'hd' }] }, { tab: { id: 987655 } }).then(() => offersFor(987655, 136)).then((o) => o.length)"
            Check "addresses that are not Facebook's own are not accepted" (Invoke-Js $sw $forged) 0
            Invoke-Js $sw "chrome.storage.session.remove(['fb:987654', 'fb:987655']).then(() => 'cleaned')" | Out-Null

            Write-Host "  RDM is not running"
            Stop-TestApp
            Check "the application has quit" (-not (Get-Process rdm -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$stage*" })) $true
            Start-Sleep -Seconds 2
            Start-PageDownload $page "$files/while-closed.bin"
            Check "the host starts RDM and hands the download over" (Wait-Log "add request from the browser extension: $files/while-closed.bin" 30)
            Check "RDM is running again" ($null -ne (Get-Process rdm -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$stage*" })) $true
            Start-Sleep -Seconds 2
            $kept = @(Get-ChildItem $downloads -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "while-closed*" })
            Check "and the browser did not download it as well" $kept.Count 0

            Write-Host "  A download link that opens a new tab"
            $tabsBefore = Get-TabCount
            Start-PageDownload $page "$files/in-new-tab.bin" -NewTab
            Check "it goes to RDM" (Wait-Log "add request from the browser extension: $files/in-new-tab.bin" 25)
            Start-Sleep -Seconds 3
            Check "and no empty tab is left behind" (Get-TabCount) $tabsBefore

            # Last, show that the page really does what the button had to survive: a click
            # anywhere else on the player goes to the page, which tries to open a tab.
            # (Whether the tab appears is up to the browser's pop-up blocker.)
            Send-Click $page 120 300
            Start-Sleep -Seconds 2
            $hijack = Invoke-Js $page "JSON.stringify(window.hijack)" | ConvertFrom-Json
            Check "(the test page does grab ordinary clicks to open a tab)" (($hijack.seen -ge 3) -and ($hijack.opened -ge 1)) $true

            $sw.Dispose(); $page.Dispose()
        }

        Get-CimInstance Win32_Process -Filter "Name = 'msedge.exe'" |
            Where-Object { $_.CommandLine -like "*$profile*" } |
            ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    }

    Stop-TestApp
    Check "application quits" (-not (Get-Process rdm -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$stage*" })) $true

    Write-Host "Unattended download (/n /q)"
    $qdir = Join-Path $work "unattended"
    $q = Start-Process $exe -ArgumentList "/n", "/q", "/d", "$files/quit-when-done.bin", "/p", $qdir -PassThru
    Check "downloads without asking and exits by itself" ($q.WaitForExit(40000))
    if (-not $q.HasExited) { $q.Kill() }
    $qfile = Join-Path $qdir "quit-when-done.bin"
    Check "the file was saved where /p said" (Test-Path $qfile)
    if (Test-Path $qfile) {
        Check "and is complete" (Get-Item $qfile).Length (5 * 1024 * 1024)
        $zone = Get-Content -Path $qfile -Stream Zone.Identifier -ErrorAction SilentlyContinue
        Check "and is marked as downloaded from the Internet" ("$zone" -like "*ZoneId=3*") $true
    }
}
finally {
    Get-Process rdm -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$stage*" } | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($registered) {
        Start-Process $rdmHost -ArgumentList "--unregister" -Wait
        foreach ($key in $browserKeys) {
            if ($previous[$key]) {
                New-Item $key -Force | Out-Null
                Set-ItemProperty $key -Name "(default)" -Value $previous[$key]
            }
        }
    }
    if (-not $server.HasExited) { $server.Kill() }
}

Write-Host "--- rdm.log ---"
Get-Content $log | ForEach-Object { Write-Host "  $_" }
Start-Sleep -Seconds 1
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
Write-Host "passed $script:pass, failed $script:fail"
if ($script:fail -gt 0) { exit 1 }
