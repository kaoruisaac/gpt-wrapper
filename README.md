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

## Usage

1. Log in to ChatGPT manually inside the app.
2. Use the `+` button to open additional ChatGPT tabs in the same app window.
3. Click a tab to switch without reloading it, or use its `×` button to close it.
4. Hold `Ctrl + 0` on either the main keyboard row or numeric keypad.
5. When the app is visible, dictation starts in the current active tab.
6. Speak while holding the shortcut, then release it to finish dictation.
7. By default, the app waits for transcription and submits the prompt in the same tab where dictation started. In **Settings → General**, you can turn off **PTT shortcuts 自動送出訊息**. The transcript then stays in the composer for editing and manual Send. This setting applies to foreground and background PTT.
8. Press **Ctrl + .** on the main keyboard or **Ctrl + Numpad .** (numeric keypad decimal key) to explicitly request Send in the active ChatGPT tab. Both shortcuts perform the same action: find the visible, enabled `aria-label="Send"` / `aria-label="傳送"` button and click it, without emulating Enter. They work regardless of the PTT auto-submit setting. Release a held PTT first; Submit shortcuts are ignored while the app is tray-hidden.

Open **Settings → 專案書籤** from the tray menu to save ChatGPT Project URLs. Choose a **Default project** to use it for new tabs, including the first tab after restart and the standby tab prepared while the app is hidden. Existing tabs stay on their current pages. Selecting **chatgpt.com** restores the built-in default. Each bookmark can have a **Message Prefix**, which is prefilled in the composer when a new tab for that project opens. The prefix is inserted once; foreground PTT does not reset it after you edit or remove it.

Press and release the tab bar **+** to open a tab with the current Default project. Hold **+** for about 400 ms to open the Project Bookmark menu, then drag to a project and release to open it in a new tab. Release outside the menu to cancel. Choosing a project here does not change the Default project.

Closing the main window hides GPT Wrapper to the system tray and preloads a hidden standby ChatGPT tab for the Default project. If PTT is pressed while the app is hidden, GPT Wrapper restores the window, promotes that standby into a new active tab, fills its Message Prefix, and then starts dictation. Reopening from the tray normally keeps the existing active tab and discards the unused standby. Choose **Quit** from the tray menu to exit completely.

The first dictation attempt may show the WebView2 microphone permission prompt. Allow it once.

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
  -> if PTT auto submit is enabled (default), wait for transcript in the composer and click Send
  -> otherwise leave the transcript in the composer for manual Send

Ctrl+. / Ctrl+NumpadDecimal pressed (independent explicit Submit)
  -> capture the active visible ChatGPT tab
  -> use the same guarded DOM Send action as Android BLE Submit
  -> only click the available Send control when the draft is ready
```

The ChatGPT Web DOM is not a public API, so its adapter selectors may need maintenance if the site UI changes.

## BLE PTT integration (Windows)

The shared controller is `handle_ptt_event` in `src-tauri/src/main.rs`. `src-tauri/src/ble_central.rs` uses Windows Runtime BLE scanning and GATT client APIs, then passes `Down(PttSource::Ble)`, `Up(PttSource::Ble)`, `SubmitBle`, and `CancelBle` directly to it on Tauri's main thread. `CancelBle` invalidates BLE-owned input and pending BLE sends without changing a keyboard-owned session. No keyboard shortcut is synthesized.

The controller captures a tab ID on DOWN. BLE UP finishes dictation in that fixed WebView and never reads the keyboard auto-submit setting. Start and Finish operations wait for their corresponding visible DOM control before the next queued PTT edge is processed. BLE Submit queues once if BLE PTT is held; otherwise it requests an explicit send in the captured WebView. `Ctrl + .` and `Ctrl + Numpad .` use the same `request_explicit_send` WebView action but target the active tab at keypress; it never synthesizes an Enter keypress or another shortcut. Both inputs locate the `aria-label="Send"` / `aria-label="傳送"` DOM button, validate it and the draft, and avoid duplicate clicks.

For dictated text, the adapter first waits for the Rust controller to verify that the Finish dictation control disappeared. It prefers an observable transcription busy-to-idle transition if exposed. Because `[data-testid="transcription-status"][aria-busy]` has **not** been validated against the live ChatGPT DOM, its absence instead activates a best-effort fallback: wait at least two seconds after Finish, require changed composer text and a further one-second stable draft with an enabled Send button. This is a practical heuristic, **not proof** that the final transcript is complete. Without a ready draft/Send within 15 seconds, the request times out and leaves text unsent. A logged click only means dispatch was requested; recording readiness and message delivery are unconfirmed. A visible window with no BLE target may submit an existing draft in the active tab; tray-hidden mode refuses this fallback.

In Settings → General → Bluetooth Controller, scan for a device advertising the [shared PTT service](android/.agent_temp/phase-0-shared-ble-contract.md), select it, and explicitly Approve/Connect. Approval stores the Windows `DeviceInformation.Id` in `ble-controller.json` under the app config directory. Only the approved ID may reconnect automatically; a changed Windows identity requires scanning and approval again. The advertised name is only a display hint. Disconnect disables reconnect until Enable is selected again; Forget removes approval. The Android V1 peripheral may use Bluetooth Just Works, so approval here is application authorization, not a claim of authenticated or encrypted pairing. The worker checks the service, Indicate property, and CCCD result before reporting Ready. It logs frame event, sequence, and uptime only.

Radio testing requires a BLE-capable Windows machine and the Android peripheral. Verify both `npm run tauri -- dev` and a packaged executable using the Phase 2 UAT list in `.agent_temp/ble-ptt-integration/phase-2-windows-ble-central.md`; the WinRT Bluetooth capability note for UWP manifests does not establish packaged Tauri behavior. Windows microphone selection and permission remain on the Windows ChatGPT WebView.

Focused checks: `cd src-tauri; cargo test --no-default-features; node tests/chatgpt_adapter.test.cjs`.
