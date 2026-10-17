# Windows installer maintenance

`installer.nsi` is the upstream template from Tauri CLI **2.12.1**:
https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.12.1/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi
Upstream license: MIT OR Apache-2.0. No SDK/dependency version was changed.

Local changes are intentionally limited to:

1. A version replacement invokes the previous NSIS uninstaller with `/UPDATE`.
   Explicit uninstall of the same version still performs normal cleanup.
2. Remove the optional recursive app-data deletion checkbox and its deletion
   block. It could delete the retained legacy `ble-controller.json` (Q13).
3. Configure the upstream pre-uninstall hook in `tauri.conf.json`, with an
   explicit `currentUser` install scope. No elevation or profile enumeration.

`hooks.nsh` stops the app through Tauri's Restart Manager check before removing
only `%APPDATA%\cc.pedelec.gptwrapper\esp32-pairs.dpapi` and `.esp32-*.tmp`.
These names match `main.rs` and `devices/store.rs`. The files contain DPAPI blobs;
their contents are never opened by the uninstaller. This is file removal, not a
claim of forensic secure erasure. On failure, uninstall stops with exit **23**,
an interactive message and (if TEMP is writable) guidance in
`%TEMP%\cc.pedelec.gptwrapper-esp32-uninstall-E23.log`. Retrying is safe even after
some pairing files have been deleted. `/UPDATE` skips pairing cleanup entirely.
Preferences, old Android data, device NVS, Bluetooth bonds and VB-CABLE are kept.

Build from the repository root:

```powershell
npm run tauri -- build --ci --bundles nsis -- --locked
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-esp32-installer.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-esp32-package.ps1
```

The second command compiles and executes the production hook in UUID-named
fixture directories, including real Windows file-sharing lock and directory-list ACL failures. It
does not delete the user's real pairing. It is engineering coverage, not full
installer/App/hardware UAT. The script requires the NSIS tools downloaded by a
Tauri bundle build and its generated `utils.nsh`.

The package test compiles the entire rendered installer with an isolated UUID
package identity, runs real install, `/UPDATE` uninstall, reinstall and normal
uninstall, verifies the installed executable SHA and retained preferences. It
never launches that executable (its compiled App identity is still production).
Interactive upgrade choices and real App DPAPI/Ready/hardware UAT remain separate.

When updating Tauri CLI, compare the new upstream template with this pinned
source, reapply these changes, and rebuild/retest the actual installer. Verify
`UpdateMode`, `PageLeaveReinstall`, app-stop ordering and the absence of recursive
profile deletion. The release workflow merges its version/target override into
this config, retaining the template and hooks. Do not release from the old build.

Release UAT, environment fields and manual installation/update/uninstall cases:
`.agent_temp/esp32-product-integration-requirements/phase-5-uat-v1.md`.
