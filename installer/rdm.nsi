; Rapid Download Manager - Windows installer (NSIS 3, Modern UI 2).
;
; Per-user install: no administrator rights, nothing written outside the
; user's profile and HKCU. Built by scripts\package.ps1, which passes
; VERSION, STAGE (folder with the files to install), ICON and OUTFILE, and
; SIGN_SCRIPT when the build is signed.

Unicode true
SetCompressor /SOLID lzma
ManifestDPIAware true
RequestExecutionLevel user

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef STAGE
  !define STAGE "..\dist\stage"
!endif
!ifndef ICON
  !define ICON "..\dist\tmp\rdm.ico"
!endif
!ifndef OUTFILE
  !define OUTFILE "..\dist\RDM-Setup.exe"
!endif

!define APP        "Rapid Download Manager"
!define PUBLISHER  "boulbeba115"
!define WEBSITE    "https://github.com/boulbeba115/Rapid-Download-Manager-RDM-"
!define REG_APP    "Software\RapidDownloadManager"
!define REG_UNINST "Software\Microsoft\Windows\CurrentVersion\Uninstall\RapidDownloadManager"
!define REG_RUN    "Software\Microsoft\Windows\CurrentVersion\Run"
!define RUN_VALUE  "RapidDownloadManager"

Name "${APP}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP}"
InstallDirRegKey HKCU "${REG_APP}" "InstallDir"
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName"     "${APP}"
VIAddVersionKey "FileDescription" "${APP} Setup"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "CompanyName"     "${PUBLISHER}"
VIAddVersionKey "LegalCopyright"  "Copyright (c) 2026 ${PUBLISHER}. MIT License."

; Authenticode: the uninstaller is generated inside the installer, so it has
; to be signed at the moment it is written, and the installer once it is
; complete. Both go through scripts\sign.ps1, which fails the build if a
; signature cannot be made.
!ifdef SIGN_SCRIPT
  !uninstfinalize 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${SIGN_SCRIPT}" "%1"' = 0
  !finalize       'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "${SIGN_SCRIPT}" "%1"' = 0
!endif

; ---------------------------------------------------------------- Pages

!define MUI_ICON   "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_FINISHPAGE_RUN "$INSTDIR\rdm.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"
; The second checkbox of the last page ("show readme" in MUI's terms).
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Show how to add the browser extension"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION ShowBrowserSetup
!define MUI_FINISHPAGE_LINK "Project page and documentation"
!define MUI_FINISHPAGE_LINK_LOCATION "${WEBSITE}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE}\LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------- Helpers

; Force-close one program of THIS installation, found by its full path - never
; other copies of RDM (a portable one, say) that happen to share the file name.
; CIM is used because it can see 64-bit processes from this 32-bit installer.
!macro KillInstalled FILE
  ; The path travels in the environment, so no quoting of it can go wrong.
  System::Call 'kernel32::SetEnvironmentVariable(t "RDM_KILL_PATH", t "$INSTDIR\${FILE}")'
  nsExec::Exec '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command "Get-CimInstance Win32_Process | Where-Object { $$_.ExecutablePath -eq $$env:RDM_KILL_PATH } | ForEach-Object { Stop-Process -Id $$_.ProcessId -Force }"'
  Pop $3
!macroend

; RDM keeps running in the notification area, so files may be in use. Ask the
; running instance to exit cleanly (it saves its state), wait for it, and only
; force it as a last resort.
!macro CloseRunningApp
  FindWindow $0 "" "${APP}"
  ${If} $0 != 0
    DetailPrint "Closing ${APP}..."
    ${If} ${FileExists} "$INSTDIR\rdm.exe"
      ExecWait '"$INSTDIR\rdm.exe" /quit'
    ${EndIf}
  ${EndIf}

  ; The process outlives its window for a moment while it saves its state,
  ; so wait until the executable can really be replaced.
  StrCpy $1 0
  ${Do}
    StrCpy $2 0
    ${If} ${FileExists} "$INSTDIR\rdm.exe"
      ClearErrors
      FileOpen $3 "$INSTDIR\rdm.exe" a
      ${If} ${Errors}
        StrCpy $2 1
      ${Else}
        FileClose $3
      ${EndIf}
    ${EndIf}
    ${If} $2 == 0
      ${ExitDo}
    ${EndIf}
    IntOp $1 $1 + 1
    ${If} $1 == 40
      ; Ten seconds of asking nicely were not enough.
      !insertmacro KillInstalled "rdm.exe"
    ${EndIf}
    ${If} $1 > 48
      ${ExitDo}
    ${EndIf}
    Sleep 250
  ${Loop}
!macroend

; The browser starts rdm-host.exe for a moment whenever the extension has
; something to say; give one that is running right now time to finish.
!macro WaitForHost
  StrCpy $1 0
  ${Do}
    ${IfNot} ${FileExists} "$INSTDIR\rdm-host.exe"
      ${ExitDo}
    ${EndIf}
    ClearErrors
    FileOpen $3 "$INSTDIR\rdm-host.exe" a
    ${IfNot} ${Errors}
      FileClose $3
      ${ExitDo}
    ${EndIf}
    IntOp $1 $1 + 1
    ${If} $1 > 40
      !insertmacro KillInstalled "rdm-host.exe"
      ${ExitDo}
    ${EndIf}
    Sleep 250
  ${Loop}
!macroend

Function ShowBrowserSetup
  ; Starts RDM with the guide open, or asks the copy that the first checkbox
  ; just started to show it.
  Exec '"$INSTDIR\rdm.exe" --browser-setup'
FunctionEnd

; ---------------------------------------------------------------- Install

Section "${APP} (required)" SecCore
  SectionIn RO
  !insertmacro CloseRunningApp
  !insertmacro WaitForHost

  SetOutPath "$INSTDIR"
  File "${STAGE}\rdm.exe"
  File "${STAGE}\rdmc.exe"
  File "${STAGE}\rdm-host.exe"
  File "${STAGE}\LICENSE.txt"
  File "${STAGE}\README.md"
  SetOutPath "$INSTDIR\extension"
  File "${STAGE}\extension\manifest.json"
  File "${STAGE}\extension\background.js"
  File "${STAGE}\extension\content.js"
  File "${STAGE}\extension\page-hook.js"
  File "${STAGE}\extension\popup.html"
  File "${STAGE}\extension\popup.css"
  File "${STAGE}\extension\popup.js"
  SetOutPath "$INSTDIR\extension\icons"
  File "${STAGE}\extension\icons\16.png"
  File "${STAGE}\extension\icons\32.png"
  File "${STAGE}\extension\icons\48.png"
  File "${STAGE}\extension\icons\128.png"
  SetOutPath "$INSTDIR"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${REG_APP}" "InstallDir" "$INSTDIR"

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortcut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\rdm.exe"
  CreateShortcut "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk" "$INSTDIR\uninstall.exe"

  ; Apps & features entry.
  WriteRegStr   HKCU "${REG_UNINST}" "DisplayName"     "${APP}"
  WriteRegStr   HKCU "${REG_UNINST}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKCU "${REG_UNINST}" "Publisher"       "${PUBLISHER}"
  WriteRegStr   HKCU "${REG_UNINST}" "URLInfoAbout"    "${WEBSITE}"
  WriteRegStr   HKCU "${REG_UNINST}" "DisplayIcon"     "$INSTDIR\rdm.exe"
  WriteRegStr   HKCU "${REG_UNINST}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKCU "${REG_UNINST}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr   HKCU "${REG_UNINST}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${REG_UNINST}" "NoModify" 1
  WriteRegDWORD HKCU "${REG_UNINST}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${REG_UNINST}" "EstimatedSize" "$0"
SectionEnd

; Tells Chrome, Edge, Brave and Chromium where RDM's native-messaging host is
; (a manifest next to the program plus one HKCU key per browser). This is what
; lets RDM's extension - and only that extension - reach the application.
Section "Browser integration (Chrome, Edge, Brave)" SecBrowser
  nsExec::ExecToLog '"$INSTDIR\rdm-host.exe" --register'
  Pop $0
  ${If} $0 != 0
    DetailPrint "Browser integration could not be registered (code $0). Use Help, Browser integration in ${APP}."
  ${EndIf}
SectionEnd

Section "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\rdm.exe"
SectionEnd

Section /o "Start with Windows" SecAutostart
  WriteRegStr HKCU "${REG_RUN}" "${RUN_VALUE}" '"$INSTDIR\rdm.exe" --minimized'
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      "The application, the command-line tool (rdmc) and the browser extension files."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecBrowser}   "Connect the browsers to RDM so its extension can hand over downloads and videos."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop}   "Put a shortcut on the desktop."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAutostart} "Start RDM in the notification area when you sign in."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------- Uninstall

Section "Uninstall"
  !insertmacro CloseRunningApp
  !insertmacro WaitForHost

  ; While the program that knows how is still there: remove the browser registration.
  ${If} ${FileExists} "$INSTDIR\rdm-host.exe"
    nsExec::ExecToLog '"$INSTDIR\rdm-host.exe" --unregister'
    Pop $0
  ${EndIf}

  ; Only what the installer put there; never a recursive delete of $INSTDIR.
  Delete "$INSTDIR\rdm.exe"
  Delete "$INSTDIR\rdmc.exe"
  Delete "$INSTDIR\rdm-host.exe"
  Delete "$INSTDIR\rdm-host.json"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\extension\manifest.json"
  Delete "$INSTDIR\extension\background.js"
  Delete "$INSTDIR\extension\content.js"
  Delete "$INSTDIR\extension\page-hook.js"
  Delete "$INSTDIR\extension\popup.html"
  Delete "$INSTDIR\extension\popup.css"
  Delete "$INSTDIR\extension\popup.js"
  Delete "$INSTDIR\extension\icons\16.png"
  Delete "$INSTDIR\extension\icons\32.png"
  Delete "$INSTDIR\extension\icons\48.png"
  Delete "$INSTDIR\extension\icons\128.png"
  RMDir  "$INSTDIR\extension\icons"
  RMDir  "$INSTDIR\extension"
  Delete "$INSTDIR\uninstall.exe"
  RMDir  "$INSTDIR"

  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  Delete "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk"
  RMDir  "$SMPROGRAMS\${APP}"
  Delete "$DESKTOP\${APP}.lnk"

  DeleteRegValue HKCU "${REG_RUN}" "${RUN_VALUE}"
  DeleteRegKey   HKCU "${REG_UNINST}"
  DeleteRegKey   HKCU "${REG_APP}"

  ; Settings and the download list are the user's data: keep them unless asked.
  ; A silent uninstall keeps them.
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also delete your RDM settings and download list?$\r$\n$\r$\nYour downloaded files are never touched." /SD IDNO IDNO keep
    RMDir /r "$APPDATA\RDM"
  keep:
SectionEnd
