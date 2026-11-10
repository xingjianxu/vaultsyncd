vaultsyncd Windows 11 x64 ZIP — UNSIGNED TEST BUILD

This is install-free, NOT a portable profile. Extract ALL files into a new local program
folder outside %LOCALAPPDATA%\vaultsyncd. Chinese characters and spaces are supported.
Do not run inside the ZIP viewer. Do not overwrite, move or rename the folder after first use.

Double-click "Start vaultsyncd.cmd". Microsoft Edge WebView2 Evergreen Runtime is required:
https://developer.microsoft.com/microsoft-edge/webview2/
The launcher checks the runtime, validates component hashes, starts the agent, waits for
real authenticated IPC health and then opens the desktop. Nothing is downloaded implicitly.
Closing the desktop DOES NOT stop synchronization. Repeating Start reuses the same agent.
No startup registration, Windows service or Start Menu shortcut is created by default.
System notifications default to OFF for a new ZIP profile. Explicitly enabling them in
Problems/Recovery opts into creating this application's Start Menu shortcut for Toasts.

You may also open vaultsyncd-desktop.exe directly. It reuses the healthy session agent
or starts the adjacent agent after the same ZIP ownership and maintenance checks,
then opens the GUI only after IPC is ready. Startup failures appear in a native dialog.

State remains on THIS Windows user account in %LOCALAPPDATA%\vaultsyncd\agent\
(agent.json, state\, agent.log, zip-stop-* recovery exports). Credentials remain in Windows
Credential Manager. Copying the program folder to another machine does not copy this profile.
Local ownership: %LOCALAPPDATA%\vaultsyncd\zip-owner.json pins the original program path
and full ZIP component manifest. Only that exact program folder may use this profile; another
extraction path is refused. Extracting a complete package over the same folder is accepted: the
next start re-pins the manifest and appends one line to zip-owner-adoptions.jsonl. That adoption
keeps NO program or state backup and does not drain transfers or export unconfirmed content, so it
cannot be rolled back, and any version direction (including a downgrade) is accepted.
Existing installed/unknown profiles and different ZIP paths are explicitly refused.
Keep the original program folder. Right-click the tray and select Auto update (自动更新)
to download the latest stable Windows ZIP from this project's GitHub Release and perform
controlled upgrade/restart in that same folder. All bindings stay paused during trial startup;
their original pause settings return after IPC health succeeds. Program, state and owner
backups are retained. An interrupted update requires the retained candidate agent's
--auto-update-recover --program-dir <original-folder> entry. Conversion to/from setup-managed
state is unsupported. Never delete owner/profile/cache or hand-mix binaries with a manifest.

Double-click "Stop vaultsyncd.cmd" and confirm Y for safe stopping. It uses PrepareMaintenance
IPC: pauses all bindings persistently, disconnects/drains callbacks, exports unconfirmed
content, waits for the authenticated agent to exit and confirms the writer lock is free.
On next Start, bindings remain PAUSED: explicitly Resume each desired binding in GUI/CLI.
A failed stop can have partially paused/disconnected bindings; inspect agent.log, do not kill
by process name. Health/stop automation: vaultsyncd-zip.exe health / vaultsyncd-zip.exe stop.
Stop never removes Root/state/cache/dirty files and never unregisters the CFAPI provider.
Cloud-only and partially downloaded placeholders are NOT complete offline files after Stop.
Restart the original ZIP when access to these placeholders is needed (Resume as appropriate).

To remove only program files: close the GUI, complete safe Stop, then delete only the
extracted program folder if no longer needed. All local state, roots, cache, unconfirmed
content and cloud placeholders stay on this machine. Keep/recover the original folder
before needing synchronization again; retaining the original ZIP archive is recommended.
There is no data cleanup or provider unregistration command in this package.

Hashes are provenance/integrity evidence, NOT a digital signature. No production signing
identity was provided. Apply your organization's policy for unsigned software; do not
remove security controls or mistake this test package for a signed production release.
