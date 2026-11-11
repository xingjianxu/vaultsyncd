vaultsyncd — Windows 11 x64

Extract the complete download to any ordinary local folder, then run
vaultsyncd-desktop.exe or Start vaultsyncd.cmd. Do not run inside the ZIP browser.
No administrator access or installer is required. Microsoft Edge WebView2
Evergreen Runtime must be available for the desktop UI.

There is no vaultsyncd-zip.exe. Normal startup does not read or verify
zip-files.json, installed-files.json, zip-owner.json, program hashes or sources.
You can move/rename the program folder or replace its files. Existing legacy
zip-owner.json files are ignored; do not remove local state to fix startup.
The manifests are release/explicit-update inventories, NOT startup requirements.

State stays with the current Windows user, not with the program folder:
  %LOCALAPPDATA%\vaultsyncd\agent\agent.json
  %LOCALAPPDATA%\vaultsyncd\agent\state\
  %LOCALAPPDATA%\vaultsyncd\agent\agent.log
Credentials remain in Windows Credential Manager. Vault/cache/user files stay
at their configured locations. Moving the program does not move these files
or change the device identity. Do not copy local state to another machine.

Only one agent writes a user's state. Opening another copy reuses the healthy
agent already running for this user; it does not replace the running version.
Close the desktop, safely stop the agent, then open the desired copy to switch
versions. By default, closing the desktop hides it in the tray and keeps syncing.
Program Settings can instead exit the desktop, optionally stopping the agent
(enabled by default). Tray Exit follows the same agent-stop preference. Safe
stop preserves recovery exports and pauses bindings; resume them after restart.
If safe stop fails, the desktop stays open and shows the reason.

Stop vaultsyncd.cmd asks for confirmation, pauses bindings, exports unconfirmed
content and safely stops the agent. Bindings remain PAUSED after restart;
resume the desired bindings in the UI. It does not delete Vaults/cache/state,
remove credentials or unregister the cloud provider. Never force-kill an agent
or delete local state to bypass a pending maintenance transaction.

Automation (quote the program path; redirect output to files, not a pipeline):
  vaultsyncd-agent.exe --user-session start
  vaultsyncd-agent.exe --user-session health
  vaultsyncd-agent.exe --user-session stop

Manual replacement/moving has no program or whole-state rollback/backup.
An older binary may reject a newer database schema. Automatic update retains
its explicit download validation, offline backups and recovery transaction;
normal startup still refuses interrupted maintenance until it is recovered.
Missing DLLs, unsupported Windows, filesystem permissions and invalid config
can still produce real operating-system/application errors; arbitrary missing
or corrupted executables cannot be made runnable without their actual bytes.

This is an unsigned test build, not a production-signed distribution. A hash
inventory is not a digital signature. First release, native CFAPI, multi-client,
performance and capacity acceptance requirements remain independent.
