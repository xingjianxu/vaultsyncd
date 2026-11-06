; -*- coding: utf-8 -*-
Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "WordFunc.nsh"
!ifndef PRODUCT_VERSION
  !error "PRODUCT_VERSION must come from workspace.package.version"
!endif
!ifndef PAYLOAD
  !error "PAYLOAD must be the same-source built client directory"
!endif
!ifndef OUTPUT
  !error "OUTPUT must be a new output file"
!endif
Name "vaultsyncd ${PRODUCT_VERSION} (unsigned test)"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\vaultsyncd"
RequestExecutionLevel user
SetCompressor /SOLID lzma
VIProductVersion "${PRODUCT_VERSION}.0"
VIAddVersionKey "ProductName" "vaultsyncd"
VIAddVersionKey "ProductVersion" "${PRODUCT_VERSION}"
VIAddVersionKey "FileVersion" "${PRODUCT_VERSION}"
VIAddVersionKey "FileDescription" "vaultsyncd unsigned test installer"
VIAddVersionKey "LegalCopyright" "vaultsyncd contributors"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_LANGUAGE "English"
Var RuntimeVersion
Function .onInit
  SetShellVarContext current
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "Windows 11 x64 is required." /SD IDOK
    SetErrorLevel 21
    Quit
  ${EndIf}
  ${IfNot} ${AtLeastWin11}
    MessageBox MB_OK|MB_ICONSTOP "Windows 11 is required." /SD IDOK
    SetErrorLevel 21
    Quit
  ${EndIf}
  SetRegView 64
  ReadRegStr $0 HKCU "Software\vaultsyncd" "InstallDir"
  ${If} $0 != ""
    MessageBox MB_OK|MB_ICONSTOP "Existing installation found. Preserve its data and use the controlled upgrade workflow; this installer only accepts a fresh installation." /SD IDOK
    SetErrorLevel 22
    Quit
  ${EndIf}
  ReadRegStr $RuntimeVersion HKCU "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ${VersionCompare} "$RuntimeVersion" "0.0.0.0" $0
  ${If} $0 != 1
    SetRegView 32
    ReadRegStr $RuntimeVersion HKLM "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
    ${VersionCompare} "$RuntimeVersion" "0.0.0.0" $0
    ${If} $0 != 1
      MessageBox MB_OK|MB_ICONSTOP "Microsoft Edge WebView2 Runtime is required. Install the Evergreen Runtime from https://developer.microsoft.com/microsoft-edge/webview2/ and run setup again. No application files or startup entries were installed." /SD IDOK
      SetErrorLevel 20
      Quit
    ${EndIf}
  ${EndIf}
  SetRegView 64
FunctionEnd
Section "vaultsyncd client"
  ; Only a new program directory is allowed. Never overwrite data or an existing installation.
  IfFileExists "$INSTDIR\*.*" invalid_directory
  StrLen $0 "$LOCALAPPDATA\vaultsyncd\"
  StrCpy $1 "$INSTDIR\" $0
  StrCmp $1 "$LOCALAPPDATA\vaultsyncd\" invalid_directory
  ClearErrors
  SetOutPath "$INSTDIR"
  File "${PAYLOAD}/vaultsyncd-desktop.exe"
  File "${PAYLOAD}/vaultsyncd-agent.exe"
  File "${PAYLOAD}/vaultsyncd-session.exe"
  File "${PAYLOAD}/vaultsyncd-cli.exe"
  File "${PAYLOAD}/installed-files.json"
  File "${PAYLOAD}/UNSIGNED-TEST.txt"
  IfErrors copy_failed
  ExecWait '"$INSTDIR\vaultsyncd-desktop.exe" --register-shortcut' $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "The user Start Menu shortcut could not be registered. No startup entry was enabled; program files were retained for diagnosis." /SD IDOK
    SetErrorLevel 25
    Quit
  ${EndIf}
  WriteRegStr HKCU "Software\vaultsyncd" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\vaultsyncd" "Version" "${PRODUCT_VERSION}"
  WriteRegStr HKCU "Software\vaultsyncd" "WebView2Version" "$RuntimeVersion"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent" '$\"$INSTDIR\vaultsyncd-session.exe$\"'
  IfErrors copy_failed
  ExecWait '"$INSTDIR\vaultsyncd-session.exe"' $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "The agent startup entry was saved but its first launch failed. Program files and LocalAppData state were preserved; review the user profile and diagnostics." /SD IDOK
    SetErrorLevel 26
    Quit
  ${EndIf}
  DetailPrint "Installed unsigned test client. Login starts an independent agent; closing the GUI keeps it running."
  Goto complete
invalid_directory:
  MessageBox MB_OK|MB_ICONSTOP "Select a new program directory outside vaultsyncd LocalAppData state. Existing content is never overwritten." /SD IDOK
  SetErrorLevel 23
  Quit
copy_failed:
  MessageBox MB_OK|MB_ICONSTOP "Installation failed. Existing data and partial program files were preserved for diagnosis." /SD IDOK
  SetErrorLevel 24
  Quit
complete:
SectionEnd
