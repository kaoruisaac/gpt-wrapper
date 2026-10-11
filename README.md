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
7. By default, the app waits for transcription and submits the prompt in the same tab where dictation started. In **Settings → General**, you can turn off **PTT shortcuts 自動送出訊息**. When ChatGPT exposes a dictation-only Finish control, the transcript stays in the composer for editing and manual Send. This setting applies to keyboard and BLE PTT, both foreground and background.
8. Press **Ctrl + .** on the main keyboard or **Ctrl + Numpad .** (numeric keypad decimal key) to explicitly request Send in the active ChatGPT tab when the main window is visible, minimized, or unfocused. Both shortcuts use the guarded `aria-label="Send"` / `aria-label="傳送"` button (never Enter), regardless of the PTT auto-submit setting. Release a held PTT first. **When the main window is hidden to the system tray** (including autostart tray-only mode), the same shortcuts and the Android BLE Submit button instead operate the **Default Project switcher**: the first press displays the current Default Project in a small non-activating toast above the Windows taskbar, and subsequent presses within three seconds cycle through ChatGPT.com and the saved Project Bookmarks in Settings order. Each selection is saved immediately; three seconds after the last press the toast disappears, and the next press again only displays the selection. No chat message is sent or existing tab changed during background switching.

Open **Settings → 專案書籤** from the tray menu to save ChatGPT Project URLs. Choose a **Default project** to use it for new tabs, including the first tab after restart and the standby tab prepared while the app is hidden. Existing tabs stay on their current pages. Selecting **chatgpt.com** restores the built-in default. Each bookmark can have a **Message Prefix**, which is prefilled in the composer when a new tab for that project opens. The prefix is inserted once; foreground PTT does not reset it after you edit or remove it.

Press and release the tab bar **+** to open a tab with the current Default project. Hold **+** for about 400 ms to open the Project Bookmark menu, then drag to a project and release to open it in a new tab. Release outside the menu to cancel. Choosing a project here does not change the Default project.

Closing the main window hides GPT Wrapper to the system tray and preloads a hidden standby ChatGPT tab for the Default project. If PTT is pressed while the app is hidden, GPT Wrapper restores the window, promotes that standby into a new active tab, fills its Message Prefix, and then starts dictation. Reopening from the tray normally keeps the existing active tab and discards the unused standby. Choose **Quit** from the tray menu to exit completely.

The first dictation attempt may show the WebView2 microphone permission prompt. Allow it once.

### ChatGPT Dictation and Voice Mode microphone selection (Voice physical audio UAT pending)

In **Settings → General → ChatGPT 麥克風**, choose **Windows Default** or a particular `audioinput` reported by the actual ChatGPT WebView, and use Refresh to rescan. The same preference controls Dictation (manual, keyboard/BLE PTT) and newly started Voice Mode capture. Allow ChatGPT's native microphone permission first if device names are hidden. The choice is persisted for all ChatGPT tabs and reloaded on startup.

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
  -> use the same guarded DOM Send action as Android BLE Submit
  -> only click the available Send control when the draft is ready

Ctrl+. / Ctrl+NumpadDecimal / BLE Submit while main window is tray-hidden
  -> first press shows the saved Default Project (no change)
  -> further presses within the same 3-second session save the next Default Project
  -> update a single non-activating taskbar-area toast and refresh standby PTT
  -> never send a ChatGPT message or activate the main window
```

The ChatGPT Web DOM is not a public API, so its adapter selectors may need maintenance if the site UI changes.

## BLE PTT integration (Windows)

The shared controller is `handle_ptt_event` in `src-tauri/src/main.rs`. `src-tauri/src/ble_central.rs` uses Windows Runtime BLE scanning and GATT client APIs, then passes `Down(PttSource::Ble)`, `Up(PttSource::Ble)`, `SubmitBle`, and `CancelBle` directly to it on Tauri's main thread. `CancelBle` invalidates BLE-owned input and pending BLE sends without changing a keyboard-owned session. No keyboard shortcut is synthesized.

The controller captures a tab ID on DOWN. **Keyboard and BLE PTT share the same auto-submit preference and finish logic**: UP first completes dictation in the fixed WebView, then requests guarded composer Send only when Settings → General → PTT auto-submit is enabled. The optional separate BLE Submit button remains explicit even with auto-submit disabled: it queues once if BLE PTT is held, otherwise requests a send in the captured WebView. Start and Finish operations wait for their corresponding visible DOM control before the next queued PTT edge is processed. The observed `停止聽寫` / `Stop dictation` accessible controls are the first-choice transcription-only Finish targets. Dictation-only Finish actions are preferred; controls labeled `Transcribe and send` / `轉錄並傳送` are used only when an automatic or explicitly queued send is allowed. If ChatGPT exposes no safe dictation-only completion control with auto-submit disabled, the app refuses to click a send-on-finish control and logs an unresolved Finish rather than silently submitting the message. `Ctrl + .` and `Ctrl + Numpad .` use the same `request_explicit_send` WebView action but target the active tab at keypress; it never synthesizes an Enter keypress or another shortcut. Both inputs locate the `aria-label="Send"` / `aria-label="傳送"` DOM button, validate it and the draft, and avoid duplicate clicks.

For dictated text, the adapter first waits for the Rust controller to verify that the Finish dictation control disappeared. It prefers an observable transcription busy-to-idle transition if exposed. Because `[data-testid="transcription-status"][aria-busy]` has **not** been validated against the live ChatGPT DOM, its absence instead activates a best-effort fallback: wait at least two seconds after Finish, require changed composer text and a further one-second stable draft with an enabled Send button. This is a practical heuristic, **not proof** that the final transcript is complete. Without a ready draft/Send within 15 seconds, the request times out and leaves text unsent. A logged click only means dispatch was requested; recording readiness and message delivery are unconfirmed. A visible window with no BLE target may submit an existing draft in the active tab; in tray-hidden mode BLE Submit instead participates in the same Default Project switching session as the two keyboard shortcuts. Only an active held PTT, in-flight Start / Finish, or in-progress recovery blocks background project switching; historical failed Finish attempts are isolated to their tabs and never block other tabs or the project switcher. After an uncertain Start/Finish, the next PTT press probes that tab: a visible Start with no Finish resumes the held press; a safe Finish is clicked and verified before the user is asked to press again. Recovery never clicks `Transcribe and send` or automatically sends a draft. Any unknown state or callback timeout cancels the current recovery attempt safely and allows retry without reloading.

In Settings → General → Bluetooth Controller, scan for a device advertising the [shared PTT service](android/.agent_temp/phase-0-shared-ble-contract.md), select it, and explicitly Pair/Approve. This asks Windows to pair when `IsPaired` is false, verifies `IsPaired` afterward, and saves the resulting Windows `DeviceInformation.Id` in `ble-controller.json` under the app config directory. A legacy approved device that is not OS-paired shows `Windows pairing required`; select it once in Scan and Pair/Approve. Auto reconnect requires a paired device whose Windows-resolved ID matches the approved ID; a different ID is rejected, even if the name and service UUID match. Disconnect disables reconnect until Enable is selected again; Forget removes app approval without unpairing Windows. The worker checks the service, Indicate property, and CCCD result before reporting Ready. Identity diagnostics use short salted labels rather than raw IDs or addresses.

The pairing response alone does not prove that Windows will resolve a later private BLE address to the same bonded identity. Android must support a usable bond, and the real Stop/Start and app restart tests below must pass before claiming persistent reconnect. If this device stack cannot supply a reusable bonded identity, the safe fallback is a separately specified, versioned challenge-response app identity protocol; a matching name or service UUID is insufficient.

Radio testing requires a BLE-capable Windows machine and the Android peripheral. Verify both `npm run tauri -- dev` and a packaged executable using the Phase 2 UAT list in `.agent_temp/ble-ptt-integration/phase-2-windows-ble-central.md`; the WinRT Bluetooth capability note for UWP manifests does not establish packaged Tauri behavior. Windows microphone selection and permission remain on the Windows ChatGPT WebView.

### Persistent identity investigation (2026-10-09)

On this Windows development machine, the existing app config had `Enabled=true` and an approved ID. A read-only WinRT query of that ID reported `IsPaired=false`, `CanPair=true`. A service-filtered advertisement from the available Samsung SM-S9160 / Android 16 peripheral resolved to a **different** Windows ID, `IsPaired=false`, `CanPair=true`, with Windows address type `BluetoothAddressType(1)`. Three further Android Stop/Start rounds gave the same comparison result. No full ID, BLE address, or pairing secret was recorded. These observations demonstrate why the old exact-ID reconnect check rejected these advertisements; they do not establish whether address rotation is the sole cause. There was no pre-approval capture and no PairAsync attempt in this diagnostic run.

The updated Windows path has passed `cargo fmt --check`, `cargo check`, unit tests, and the Settings build. **Physical acceptance remains open:** Pair/Approve must be performed with the Android bonding changes, followed by ten Stop/Start rounds with no Windows Settings connection clicks, a new CCCD Indicate subscription and Down/Up/Submit in each round, Windows app restart, tray, radio Off/On, and dev plus packaged builds. If PairAsync fails or a paired advertisement still resolves to a different ID, record the pairing status and the two salted identity labels and treat persistent identity as blocked; do not reapprove an arbitrary same-name device.

Focused checks: `cd src-tauri; cargo test --no-default-features; node tests/chatgpt_adapter.test.cjs`.
