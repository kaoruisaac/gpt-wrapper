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
7. The app waits for transcription and submits the prompt in the same tab where dictation started.

Open **Settings → 專案書籤** from the tray menu to save ChatGPT Project URLs. Choose a **Default project** to use its URL for new tabs, including the first tab after restart and the standby tab prepared while the app is hidden. Existing tabs stay on their current pages. Selecting **chatgpt.com** restores the built-in default. Each bookmark also stores an optional **Message Prefix** for display and later editing; it does not change dictation or sent messages.

Press and release the tab bar **+** to open a tab with the current Default project. Hold **+** for about 400 ms to open the Project Bookmark menu, then drag to a project and release to open it in a new tab. Release outside the menu to cancel. Choosing a project here does not change the Default project.

Closing the main window hides GPT Wrapper to the system tray and preloads a hidden standby ChatGPT tab. If PTT is pressed while the app is hidden, GPT Wrapper restores the window, promotes that standby into a new active tab, and starts dictation there. Reopening from the tray normally keeps the existing active tab and discards the unused standby. Choose **Quit** from the tray menu to exit completely.

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
  -> start dictation immediately if ready, or when loading finishes while the shortcut is still held

Shortcut Released
  -> return to the tab captured on Pressed
  -> finish dictation only if recording actually started
  -> wait for transcript in the composer
  -> click Send
```

The ChatGPT Web DOM is not a public API, so its adapter selectors may need maintenance if the site UI changes.
