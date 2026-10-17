# GPT Wrapper

Windows Tauri app that keeps ChatGPT Web available as a push-to-talk dictation window with multiple tabs in one native window.

## Run

Install dependencies once:

```powershell
cd C:\workspace\Projects\gpt-wrapper
npm install
```

Start the Tauri app in development mode:

```powershell
npm run tauri dev
```

Build the app:

```powershell
npm run tauri build
```

The SolidJS tab bar and Settings UIs are built automatically by the Tauri dev/build flow; no separate frontend build step is required.

## Windows releases

Commit the release workflow and app changes, push the commit, then create and push a version tag:

```powershell
git tag v0.0.1
git push origin v0.0.1
```

The `.github/workflows/release-windows.yml` workflow runs when a `vX.X.X` tag is pushed (for example, `v0.0.1`). It builds a Windows x64 NSIS `*-setup.exe` installer and publishes it to the matching GitHub Release with automatically generated release notes. Only three-part numeric versions without leading zeroes are accepted; prerelease tags such as `v0.0.1-beta` are rejected.

The installer and app version come from the tag through a temporary Tauri config override, so you do not need to edit `package.json`, `Cargo.toml`, or `tauri.conf.json` for each release. The workflow explicitly enables NSIS bundling and uses the committed npm and Cargo lockfiles. Tauri builds both frontend UIs automatically.

GitHub Actions must be enabled for the repository. The workflow uses its built-in `GITHUB_TOKEN` with `contents: write`; no extra release token is required. Installers are currently unsigned. Successful builds also retain the installer as an Actions artifact for 14 days, including when release publishing fails. To retry a failed run, use **Actions → Release Windows NSIS → Re-run failed jobs**; an existing release receives replacement assets with the same filenames, while its title and notes are preserved.

## Usage

1. Log in to ChatGPT manually inside the app.
2. Use the `+` button to open additional ChatGPT tabs in the same app window.
3. Click a tab to switch without reloading it, or use its `×` button to close it.
4. Hold `Ctrl + 0` on either the main keyboard row or numeric keypad.
5. When the app is visible, dictation starts in the current active tab.
6. Speak while holding the shortcut, then release it to finish dictation.
7. By default, the app waits for transcription and submits the prompt in the same tab where dictation started. In **Settings → General**, you can turn off **PTT shortcuts 自動送出訊息**. When ChatGPT exposes a dictation-only Finish control, the transcript stays in the composer for editing and manual Send. This setting applies to keyboard PTT, both foreground and background.
8. Press **Ctrl + .** on the main keyboard or **Ctrl + Numpad .** (numeric keypad decimal key) to explicitly request Send in the active ChatGPT tab when the main window is visible, minimized, or unfocused. Both shortcuts use the guarded `aria-label="Send"` / `aria-label="傳送"` button (never Enter), regardless of the PTT auto-submit setting. Release a held PTT first. **When the main window is hidden to the system tray** (including autostart tray-only mode), the same shortcuts instead operate the **Default Project switcher**: the first press displays the current Default Project in a small non-activating toast above the Windows taskbar, and subsequent presses within three seconds cycle through ChatGPT.com and the saved Project Bookmarks in Settings order. Each selection is saved immediately; three seconds after the last press the toast disappears, and the next press again only displays the selection. No chat message is sent or existing tab changed during background switching.

Open **Settings → 專案書籤** from the tray menu to save ChatGPT Project URLs. Choose a **Default project** to use it for new tabs, including the first tab after restart and the standby tab prepared while the app is hidden. Existing tabs stay on their current pages. Selecting **chatgpt.com** restores the built-in default. Each bookmark can have a **Message Prefix**, which is prefilled in the composer when a new tab for that project opens. The prefix is inserted once; foreground PTT does not reset it after you edit or remove it.

Press and release the tab bar **+** to open a tab with the current Default project. Hold **+** for about 400 ms to open the Project Bookmark menu, then drag to a project and release to open it in a new tab. Release outside the menu to cancel. Choosing a project here does not change the Default project.

Closing the main window hides GPT Wrapper to the system tray and preloads a hidden standby ChatGPT tab for the Default project. If PTT is pressed while the app is hidden, GPT Wrapper restores the window, promotes that standby into a new active tab, fills its Message Prefix, and then starts dictation. Reopening from the tray normally keeps the existing active tab and discards the unused standby. Choose **Quit** from the tray menu to exit completely.

The first dictation attempt may show the WebView2 microphone permission prompt. Allow it once.

### ChatGPT Dictation and Voice Mode microphone selection (Voice physical audio UAT pending)

In **Settings → General → ChatGPT 麥克風**, choose **Windows Default** or a particular `audioinput` reported by the actual ChatGPT WebView, and use Refresh to rescan. The same preference controls Dictation (manual, keyboard PTT) and newly started Voice Mode capture. Allow ChatGPT's native microphone permission first if device names are hidden. The choice is persisted for all ChatGPT tabs and reloaded on startup.

For an explicitly chosen device, the app recognizes the **Dictation** or **Start Voice** button click and changes only classified audio captures to `deviceId: { exact: selectedId }`, preserving other audio constraints. It verifies every returned audio track's device ID; mismatches stop the Stream and reject the capture without fallback. Voice's observed multiple initial audio streams share a short, rolling capture window (8 seconds before the first request, 12 seconds between requests, maximum 45 seconds from Start Voice), rather than an indefinitely active ticket. This is a bounded DOM-gesture heuristic, **not a guaranteed Voice session protocol**. A device disappearance, permission denial, or mismatch creates an error local to that tab; the next Voice/Dictation click can retry without a persistent lock. Voice failures never cancel a PTT hold or pending Send; Dictation keeps its existing per-attempt PTT cancellation. When the preference changes, managed audio tracks are stopped; restart an ongoing Voice call to capture the new source. With **Windows Default**, native capture constraints and returned Promises remain unchanged. New tabs receive preference without proactive enumeration; only Settings initial load / Refresh and explicit selection enumerate inputs. PTT still waits for per-tab preference acknowledgement without device preflight. The affected ChatGPT tab shows a recovery alert and Settings entry.

**UAT status:** Existing Dictation and multi-tab behavior has passed user-reported manual testing (2026-10-09). **Voice Mode with two physically isolated microphones is not yet manually verified.** The classification comes from a live ChatGPT POC with two sequential Voice captures in the top frame, and may be invalidated by DOM changes, reuse of existing streams, delayed reacquisition after the 45-second classification window, or captures in other frames. Do not claim Voice input routing is physically validated until the Voice checks in `.agent_temp/voice-mode-microphone-selection/implementation.md` pass with distinct microphones and a rebuilt app. The wrapper never records, stores, or retransmits audio. Development-only capture diagnostics can be enabled with `$env:GPT_WRAPPER_MICROPHONE_POC = "1"` before a **debug** launch and disabled by removing that environment variable and restarting.

## Current implementation

```text
Single native window
  -> host WebView renders the tab strip
  -> one child WebView per ChatGPT tab
  -> hidden tabs stay alive, preserving conversation/composer state

Ctrl+0 / Ctrl+Numpad0 while visible
  -> keep the current active tab
  -> lock the dictation session to that tab
  -> start dictation with the existing trusted WebView2 input flow

Close to system tray
  -> keep normal tabs unchanged
  -> preload one hidden standby ChatGPT WebView

Ctrl+0 / Ctrl+Numpad0 while tray-hidden
  -> promote the standby into a new active tab
  -> restore/focus the native window
  -> fill the Default project's Message Prefix after the tab is ready
  -> start dictation while the shortcut is still held

Shortcut Released
  -> return to the tab captured on Pressed
  -> finish dictation only if the Start button was found and a click was dispatched
  -> prefer a dictation-only Finish button; never click a send-on-finish button when auto-submit is off
  -> if PTT auto submit is enabled (default), wait for transcript in the composer and click Send
  -> otherwise leave the transcript in the composer for manual Send (if the safe Finish control exists)

Ctrl+. / Ctrl+NumpadDecimal pressed while main window is not tray-hidden
  -> capture the active ChatGPT tab
  -> use the same guarded DOM Send action for Explicit Submit
  -> only click the available Send control when the draft is ready

Ctrl+. / Ctrl+NumpadDecimal while main window is tray-hidden
  -> first press shows the saved Default Project (no change)
  -> further presses within the same 3-second session save the next Default Project
  -> update a single non-activating taskbar-area toast and refresh standby PTT
  -> never send a ChatGPT message or activate the main window
```

The ChatGPT Web DOM is not a public API, so its adapter selectors may need maintenance if the site UI changes.

## Optional ESP32 device integration

Settings → Devices now contains the ESP32 Add Device wizard: BLE Security-1, physical identification, Wi-Fi scan/manual entry, separate password consent, built-in Rust Host authentication, and two-sided persistent commit. The user confirmed successful full real-device connection, automatic recovery after fully quitting/reopening the app, and recovery after ESP32 power cycling on 2026-10-16. Fault scenarios and the remaining hardware acceptance checks are still pending. See [firmware build, flash approval and pairing instructions](esp32/README.md). Phase 3 now includes authenticated X1 PTT/Submit/Combo, P8AU AES-GCM microphone transport, the built-in Rust CABLE bridge, and a per-Dictation WebView route lease. Firmware 0.3.0 was uploaded with explicit approval on 2026-10-16; flash hashes, crypto self-test and PDM startup passed. The user confirmed successful real App × ESP32 × ChatGPT speech transcription and Submit on 2026-10-16. Remaining Combo, fault recovery, routing and acceptance scenarios are pending; Phase 3 is not fully signed off. Pairing alone never opens the audio gate. Keyboard PTT, Explicit Submit, tabs, bookmarks, tray/standby and normal microphone selection remain available without an ESP32 or VB-CABLE.

The Android Bluetooth Controller has been retired. The app no longer starts its GATT central, receives Android control events or exposes its Scan/Approve/Disconnect/Forget/Enable commands. An existing private `ble-controller.json` is left on disk but is never read or used. Windows Bluetooth bonds are left intact. ESP32 provisioning uses its own Protocomm protocol and scans only after Add Device.

ESP32 records are stored in `esp32-pairs.dpapi` under the current user's app configuration directory. Pending transactions are quarantined after restart; an uncertain result requires physical five-second reset and re-pairing. Forget clears only the local record. The Receiver exclusively reserves UDP 3335/3333; a conflict is shown in Devices and can be retried after the other program stops. No Python tool is a runtime dependency. The Rust and Arduino SDK versions are unchanged; added crypto/XML/CPAL audio dependencies and native WLAN/session/desktop features are documented by the pairing sources and firmware guide. A reproducible public OpenSSL Security-1 fixture is generated with `node scripts/esp32-security1-fixture.cjs`.

The ESP32 security model is for personal use: Security-1 uses a shared fixed PoP
automatically supplied by the App. It cannot provide production-grade device
identity or prevent impersonation by someone who knows that PoP. Each pairing
still uses an independent random key for authenticated Wi-Fi control and encrypted
audio. First-use USB Owner enrollment and Security-2 are not required.

Installer cleanup and update retention are tracked in the Phase 5 release UAT;
full installer acceptance is pending. The current-user NSIS implementation and
E23 recovery instructions are described in [installer maintenance](src-tauri/windows/README.md).
Reinstalling after normal uninstall requires holding both ESP32 buttons for five
seconds and pairing again. VB-CABLE remains a separately installed user dependency.

PTT ownership, deferred events, queued device Submit, generation guards and Start/Finish recovery remain in the transport-independent session controller in `src-tauri/src/main.rs`. The built-in Wi-Fi adapter validates the selected device, authenticated session, sequence, press epoch and Submit intent before dispatch. Keyboard Explicit Submit keeps the guarded DOM Send action; tray-hidden Submit keeps the Default Project switcher.

Focused checks: `npm run frontend:build`, `cargo check --manifest-path src-tauri/Cargo.toml`, `cargo test --manifest-path src-tauri/Cargo.toml`, and `node --test src-tauri/tests/*.test.cjs tests/*.test.cjs`. Phase 1 regression evidence and physical UAT status are recorded in `.agent_temp/esp32-product-integration-requirements/phase-1-validation.md`.

For ESP32 Dictation, install [VB-CABLE](https://vb-audio.com/Cable/) yourself, then
use Devices → Audio Bridge → Recheck. CABLE Input is the Rust playback endpoint;
CABLE Output is resolved in the target ChatGPT origin for this Dictation only.
Fixed +12dB gain/limiter and automatic bounded jitter buffering require no sliders.
Missing audio disables ESP32 Dictation while a fresh independent authenticated
Submit remains usable. Foreground independent hardware Submit targets the current
tab; a queued intent retains its captured session/tab. Tray Submit uses the existing
Default Project switch. Disable, source changes and faults cancel without sending.

Phase 4 adds bounded Receiver recovery (three consecutive failures pause automatic
retries until Devices → Retry), current-session Windows lock/sleep/logout handling,
and safe cleanup on every App exit. Hiding to the tray leaves the Receiver running.
Interruption notices reuse the non-activating notification cards: PTT failures notify
immediately on detection; idle offline notifies once after more than ten seconds.
No notification timer delays gate closure. Reconnection requires a fresh button press.
Devices separates pairing/Host authorization from Audio readiness, and explains
physical five-second reset for Wi-Fi/Host changes and local-only Forget.
Firmware 0.4.0 adds gate-driven LED patterns and uncommitted Wi-Fi retry; build and
USB recovery are documented in `esp32/README.md`. Hardware fault UAT and flash status
are tracked in `.agent_temp/esp32-product-integration-requirements/phase-4-validation.md`;
engineering checks alone do not complete Phase 4. No SDK versions changed.
