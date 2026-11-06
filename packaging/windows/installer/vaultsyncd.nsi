; -*- coding: utf-8 -*-
Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
!include "WordFunc.nsh"
!include "FileFunc.nsh"
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
!define MUI_PAGE_CUSTOMFUNCTION_PRE DirectoryPre
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_LANGUAGE "English"
Var RuntimeVersion
Var Upgrade
Var Stage
Var PreviousRun
Var ProviderRemoval
Function DirectoryPre
  ${If} $Upgrade == 1
    Abort
  ${EndIf}
FunctionEnd
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
  StrCpy $Upgrade 0
  SetRegView 64
  ReadRegStr $0 HKCU "Software\vaultsyncd" "InstallDir"
  ${If} $0 != ""
    StrCpy $INSTDIR $0
    ReadRegStr $1 HKCU "Software\vaultsyncd" "Version"
    ${VersionCompare} "${PRODUCT_VERSION}" "$1" $2
    ${If} $2 != 1
      MessageBox MB_OK|MB_ICONSTOP "This package must be newer than the installed version. Existing data and programs were preserved." /SD IDOK
      SetErrorLevel 22
      Quit
    ${EndIf}
    StrCpy $Upgrade 1
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
  ${If} $Upgrade == 1
    ReadRegStr $PreviousRun HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent"
    ClearErrors
    CreateDirectory "$LOCALAPPDATA\vaultsyncd\maintenance"
    GetTempFileName $Stage "$LOCALAPPDATA\vaultsyncd\maintenance"
    Delete "$Stage"
    CreateDirectory "$Stage"
    SetOutPath "$Stage"
    File "${PAYLOAD}/vaultsyncd-desktop.exe"
    File "${PAYLOAD}/vaultsyncd-agent.exe"
    File "${PAYLOAD}/vaultsyncd-session.exe"
    File "${PAYLOAD}/vaultsyncd-cli.exe"
    File "${PAYLOAD}/installed-files.json"
    File "${PAYLOAD}/UNSIGNED-TEST.txt"
    IfErrors maintenance_failed
    ExecWait '"$Stage\vaultsyncd-agent.exe" --maintenance upgrade --program-dir "$INSTDIR"' $0
    IfErrors maintenance_failed
    ${If} $0 != 0
      Goto maintenance_failed
    ${EndIf}
  ${Else}
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
  ${EndIf}
  ExecWait '"$INSTDIR\vaultsyncd-desktop.exe" --register-shortcut' $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "The user Start Menu shortcut could not be registered. State and program backups were preserved." /SD IDOK
    SetErrorLevel 25
    Quit
  ${EndIf}
  WriteRegStr HKCU "Software\vaultsyncd" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\vaultsyncd" "Version" "${PRODUCT_VERSION}"
  WriteRegStr HKCU "Software\vaultsyncd" "WebView2Version" "$RuntimeVersion"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd" "DisplayName" "vaultsyncd (unsigned test)"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd" "DisplayVersion" "${PRODUCT_VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd" "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd" "NoRepair" 1
  ${If} $Upgrade == 0
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent" '$\"$INSTDIR\vaultsyncd-session.exe$\"'
  ${ElseIf} $PreviousRun != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent" "$PreviousRun"
  ${EndIf}
  IfErrors copy_failed
  ExecWait '"$INSTDIR\vaultsyncd-session.exe"' $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Program/state were preserved but agent startup requires recovery. Inspect the maintenance barrier and retained candidate; no user content was deleted." /SD IDOK
    SetErrorLevel 26
    Quit
  ${EndIf}
  DetailPrint "Installed unsigned test client. State, downloaded files and unconfirmed content are preserved."
  Goto complete
maintenance_failed:
  ${If} $PreviousRun != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent" "$PreviousRun"
  ${EndIf}
  ExecWait '"$INSTDIR\vaultsyncd-session.exe"' $0
  MessageBox MB_OK|MB_ICONSTOP "Maintenance failed. No user content was deleted. Offline state/program rollback was attempted. If maintenance.json remains, run the retained candidate agent with --maintenance recover --program-dir and the installed directory. Candidate retained at: $Stage" /SD IDOK
  SetErrorLevel 27
  Quit
invalid_directory:
  MessageBox MB_OK|MB_ICONSTOP "Select a new program directory outside vaultsyncd LocalAppData state. Existing content is never overwritten." /SD IDOK
  SetErrorLevel 23
  Quit
copy_failed:
  MessageBox MB_OK|MB_ICONSTOP "Installation failed. Existing data, backups and partial program files were preserved for diagnosis." /SD IDOK
  SetErrorLevel 24
  Quit
complete:
SectionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
  StrCpy $ProviderRemoval 0
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/PROVIDER" $1
  ${IfNot} ${Errors}
    StrCpy $ProviderRemoval 1
  ${EndIf}
  IfSilent un_init_complete
  MessageBox MB_YESNOCANCEL|MB_ICONQUESTION|MB_DEFBUTTON2 "Remove provider registration too? YES requires all files fully downloaded; cloud-only/partial files cause refusal. Complete and dirty files, cache and state are always kept. NO (default) removes programs only; cloud-only placeholders and registrations remain and cannot provide complete offline files until reinstall. CANCEL changes nothing." IDYES un_provider IDNO un_init_complete
  Abort
un_provider:
  StrCpy $ProviderRemoval 1
un_init_complete:
FunctionEnd
Section "Uninstall"
  ReadRegStr $INSTDIR HKCU "Software\vaultsyncd" "InstallDir"
  ${If} $INSTDIR == ""
    SetErrorLevel 22
    Quit
  ${EndIf}
  ReadRegStr $PreviousRun HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent"
  ClearErrors
  CreateDirectory "$LOCALAPPDATA\vaultsyncd\maintenance"
  GetTempFileName $Stage "$LOCALAPPDATA\vaultsyncd\maintenance"
  Delete "$Stage"
  CreateDirectory "$Stage"
  CopyFiles /SILENT "$INSTDIR\vaultsyncd-agent.exe" "$Stage"
  CopyFiles /SILENT "$INSTDIR\vaultsyncd-session.exe" "$Stage"
  CopyFiles /SILENT "$INSTDIR\vaultsyncd-cli.exe" "$Stage"
  CopyFiles /SILENT "$INSTDIR\vaultsyncd-desktop.exe" "$Stage"
  CopyFiles /SILENT "$INSTDIR\installed-files.json" "$Stage"
  CopyFiles /SILENT "$INSTDIR\UNSIGNED-TEST.txt" "$Stage"
  IfErrors un_failed
  ${If} $ProviderRemoval == 1
    ExecWait '"$Stage\vaultsyncd-agent.exe" --maintenance uninstall-provider --program-dir "$INSTDIR"' $0
  ${Else}
    ExecWait '"$Stage\vaultsyncd-agent.exe" --maintenance uninstall-program --program-dir "$INSTDIR"' $0
  ${EndIf}
  IfErrors un_failed
  ${If} $0 != 0
    Goto un_failed
  ${EndIf}
  Delete "$INSTDIR\vaultsyncd-agent.exe"
  Delete "$INSTDIR\vaultsyncd-session.exe"
  Delete "$INSTDIR\vaultsyncd-cli.exe"
  Delete "$INSTDIR\vaultsyncd-desktop.exe"
  Delete "$INSTDIR\installed-files.json"
  Delete "$INSTDIR\UNSIGNED-TEST.txt"
  Delete "$INSTDIR\uninstall.exe"
  IfErrors un_failed
  Delete "$SMPROGRAMS\vaultsyncd.lnk"
  DeleteRegKey HKCU "Software\vaultsyncd"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd"
  RMDir "$INSTDIR"
  DetailPrint "Programs removed. User content, state, cache and recovery exports were retained. Cloud-only placeholders are not complete files."
  Goto un_complete
un_failed:
  ${If} $PreviousRun != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "vaultsyncd-agent" "$PreviousRun"
  ${EndIf}
  ExecWait '"$INSTDIR\vaultsyncd-session.exe"' $0
  MessageBox MB_OK|MB_ICONSTOP "Uninstall refused or incomplete. User data and recovery evidence were preserved. Cloud-only/partial content must be downloaded before provider removal; program-only removal keeps those placeholders for reinstall. Recover a remaining maintenance barrier before retrying." /SD IDOK
  SetErrorLevel 27
  Quit
un_complete:
SectionEnd
