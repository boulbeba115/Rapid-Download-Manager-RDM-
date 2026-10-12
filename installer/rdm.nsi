; Rapid Download Manager - Windows installer (NSIS 3, Modern UI 2).
;
; Per-user install: no administrator rights, nothing written outside the
; user's profile and HKCU. Built by scripts\package.ps1, which passes
; VERSION, STAGE (folder with the files to install), ICON and OUTFILE.

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

; ---------------------------------------------------------------- Pages

!define MUI_ICON   "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_FINISHPAGE_RUN "$INSTDIR\rdm.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"
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
      nsExec::Exec 'taskkill /F /IM rdm.exe'
      Pop $3
    ${EndIf}
    ${If} $1 > 48
      ${ExitDo}
    ${EndIf}
    Sleep 250
  ${Loop}
!macroend

; ---------------------------------------------------------------- Install

Section "${APP} (required)" SecCore
  SectionIn RO
  !insertmacro CloseRunningApp

  SetOutPath "$INSTDIR"
  File "${STAGE}\rdm.exe"
  File "${STAGE}\rdmc.exe"
  File "${STAGE}\LICENSE.txt"
  File "${STAGE}\README.md"
  SetOutPath "$INSTDIR\extension"
  File "${STAGE}\extension\manifest.json"
  File "${STAGE}\extension\background.js"
  File "${STAGE}\extension\popup.html"
  File "${STAGE}\extension\popup.js"
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

Section "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\rdm.exe"
SectionEnd

Section /o "Start with Windows" SecAutostart
  WriteRegStr HKCU "${REG_RUN}" "${RUN_VALUE}" '"$INSTDIR\rdm.exe" --minimized'
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore}      "The application, the command-line tool (rdmc) and the browser extension files."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop}   "Put a shortcut on the desktop."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecAutostart} "Start RDM in the notification area when you sign in."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------- Uninstall

Section "Uninstall"
  !insertmacro CloseRunningApp

  ; Only what the installer put there; never a recursive delete of $INSTDIR.
  Delete "$INSTDIR\rdm.exe"
  Delete "$INSTDIR\rdmc.exe"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\extension\manifest.json"
  Delete "$INSTDIR\extension\background.js"
  Delete "$INSTDIR\extension\popup.html"
  Delete "$INSTDIR\extension\popup.js"
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
