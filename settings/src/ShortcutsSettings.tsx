import { createSignal, onCleanup, onMount, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

type Action = "ptt" | "submit" | "switchTabs";
type Settings = { ptt: string[]; submit: string[]; switchTabs: string[] };
type PlatformInfo = { os: "windows" | "macos"; devicesSupported: boolean; primaryModifier: string };
// Windows defaults; the host replaces them with its own OS defaults on load.
const WINDOWS_DEFAULTS: Settings = {
  ptt: ["Ctrl+Digit0", "Ctrl+Numpad0"],
  submit: ["Ctrl+Period", "Ctrl+NumpadDecimal"],
  switchTabs: ["Ctrl+Shift+Equal", "Ctrl+NumpadAdd"],
};
const MAC_MODIFIER_LABELS: Record<string, string> = {
  Ctrl: "⌃ Control", Alt: "⌥ Option", Shift: "⇧ Shift", Command: "⌘ Command",
};
const clone = (value: Settings): Settings => ({
  ptt: [...value.ptt], submit: [...value.submit], switchTabs: [...value.switchTabs],
});
const labels: Record<string, string> = {
  Period: ".", Comma: ",", Slash: "/", Backslash: "Backslash", Semicolon: ";",
  Quote: "'", BracketLeft: "[", BracketRight: "]", Minus: "-", Equal: "=",
  Backquote: "`", NumpadDecimal: "Num .", NumpadAdd: "Num +",
  NumpadSubtract: "Num −", NumpadMultiply: "Num ×", NumpadDivide: "Num /",
  NumpadEnter: "Num Enter", Space: "Space", Escape: "Esc",
  ArrowLeft: "←", ArrowRight: "→", ArrowUp: "↑", ArrowDown: "↓",
};
function keyName(code: string): string {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^Numpad[0-9]$/.test(code)) return "Num " + code.slice(6);
  return labels[code] || code;
}
let isMac = false;
function format(binding: string) {
  if (!isMac && binding === "Ctrl+Shift+Equal") return ["Ctrl", "+"];
  return binding.split("+").map((part) => {
    if (part === "Ctrl" || part === "Alt" || part === "Shift" || part === "Command") {
      return isMac ? MAC_MODIFIER_LABELS[part] : part;
    }
    return keyName(part);
  });
}
// Mirrors shortcut_settings.rs: the Windows set everywhere, plus Command with
// exactly one other modifier on macOS (Command alone would hijack app keys).
function modifiers(event: KeyboardEvent, mac: boolean): string {
  const { ctrlKey: c, altKey: a, shiftKey: s, metaKey: m } = event;
  if (!m) {
    return c && a && !s ? "Ctrl+Alt"
      : c && s && !a ? "Ctrl+Shift"
      : c && !s && !a ? "Ctrl"
      : a && !c && !s ? "Alt" : "";
  }
  if (!mac) return "";
  return c && !a && !s ? "Ctrl+Command"
    : a && !c && !s ? "Alt+Command"
    : s && !c && !a ? "Shift+Command" : "";
}
function supported(code: string): boolean {
  return /^Key[A-Z]$/.test(code) || /^Digit[0-9]$/.test(code) ||
    /^Numpad[0-9]$/.test(code) || /^F([1-9]|1[0-9]|2[0-4])$/.test(code) ||
    /^(Period|Comma|Slash|Backslash|Semicolon|Quote|BracketLeft|BracketRight|Minus|Equal|Backquote|Space|Tab|Enter|Backspace|Delete|Insert|Home|End|PageUp|PageDown|ArrowLeft|ArrowRight|ArrowUp|ArrowDown|NumpadDecimal|NumpadAdd|NumpadSubtract|NumpadMultiply|NumpadDivide|NumpadEnter|NumpadEqual|CapsLock|NumLock|ScrollLock|PrintScreen|Pause)$/.test(code);
}

export default function ShortcutsSettings() {
  const [defaults, setDefaults] = createSignal<Settings>(clone(WINDOWS_DEFAULTS));
  const [platform, setPlatform] = createSignal<PlatformInfo>({ os: "windows", devicesSupported: true, primaryModifier: "Ctrl" });
  const [registrationError, setRegistrationError] = createSignal("");
  const [settings, setSettings] = createSignal<Settings>(clone(WINDOWS_DEFAULTS));
  const [saved, setSaved] = createSignal(JSON.stringify(WINDOWS_DEFAULTS));
  const [busy, setBusy] = createSignal(true);
  const [status, setStatus] = createSignal("");
  const [error, setError] = createSignal("");
  const [recording, setRecording] = createSignal<{ action: Action; index: number | null } | null>(null);
  const [captureError, setCaptureError] = createSignal("");

  const read = async () => {
    setBusy(true); setError("");
    try {
      const [info, osDefaults, startupError] = await Promise.all([
        invoke<PlatformInfo>("get_platform_info"),
        invoke<Settings>("get_shortcut_defaults"),
        invoke<string | null>("get_shortcut_registration_error"),
      ]);
      isMac = info.os === "macos";
      setPlatform(info);
      setDefaults(clone(osDefaults));
      setRegistrationError(startupError ?? "");
      const value = await invoke<Settings>("get_shortcut_settings");
      setSettings(clone(value));
      setSaved(JSON.stringify(value));
    } catch (err) { setError("無法載入快捷鍵設定：" + String(err)); }
    finally { setBusy(false); }
  };
  onMount(() => { void read(); });
  const changed = () => JSON.stringify(settings()) !== saved();
  const reset = () => { setSettings(clone(defaults())); setStatus(""); setError(""); };
  const remove = (action: Action, index: number) => {
    if (settings()[action].length <= 1) return;
    setSettings(value => ({ ...value, [action]: value[action].filter((_, i) => i !== index) }));
    setStatus(""); setError("");
  };
  const start = (action: Action, index: number | null) => {
    setCaptureError(""); setRecording({ action, index });
  };
  const cancel = () => { setRecording(null); setCaptureError(""); };
  const onKeyDown = (event: KeyboardEvent) => {
    const current = recording();
    if (!current) return;
    event.preventDefault(); event.stopImmediatePropagation();
    if (event.code === "Escape" && !event.ctrlKey && !event.altKey && !event.metaKey) { cancel(); return; }
    if (["Control", "Alt", "Shift", "Meta", "AltGraph"].includes(event.key)) return;
    const mod = modifiers(event, isMac);
    if (!mod) {
      setCaptureError(isMac
        ? "請使用 ⌃、⌥、⌃⌥、⌃⇧，或 ⌘ 搭配 ⌃／⌥／⇧ 其中一個，再加一個按鍵。"
        : "請使用 Ctrl、Alt、Ctrl + Alt 或 Ctrl + Shift 搭配一個按鍵。");
      return;
    }
    if (!supported(event.code)) { setCaptureError("此按鍵不支援全域快捷鍵註冊。"); return; }
    const binding = mod + "+" + event.code;
    const duplicate = (["ptt", "submit", "switchTabs"] as Action[]).some(action =>
      settings()[action].some((item, index) => item === binding && (action !== current.action || index !== current.index)));
    if (duplicate) { setCaptureError("這組快捷鍵已被使用：" + format(binding).join(" + ")); return; }
    if (current.index === null && settings()[current.action].length >= 16) {
      setCaptureError("每個動作最多可設定 16 組快捷鍵。"); return;
    }
    setSettings(previous => ({
      ...previous,
      [current.action]: current.index === null
        ? [...previous[current.action], binding]
        : previous[current.action].map((item, index) => index === current.index ? binding : item),
    }));
    setRecording(null);
    setStatus(""); setError("");
  };
  window.addEventListener("keydown", onKeyDown, true);
  onCleanup(() => window.removeEventListener("keydown", onKeyDown, true));
  const save = async () => {
    setBusy(true); setError(""); setStatus("");
    try {
      const actual = await invoke<Settings>("set_shortcut_settings", { settings: settings() });
      setSettings(clone(actual));
      setSaved(JSON.stringify(actual));
      setStatus("快捷鍵已儲存並立即生效。");
    } catch (err) {
      setError("儲存失敗：" + String(err));
    } finally { setBusy(false); }
  };

  return (
    <>
      <h1>Shortcuts</h1>
      <p class="shortcuts-intro">自訂全域快捷鍵。點擊「變更」後，直接按下想設定的組合。</p>
      <Show when={registrationError()}><p class="shortcut-error" role="alert">{registrationError()}</p></Show>
      <Show when={platform().os === "macos"} fallback={
        <div class="shortcut-modes"><span>Ctrl + Key</span><span>Alt + Key</span><span>Ctrl + Alt + Key</span><span>Ctrl + Shift + Key</span></div>
      }>
        <div class="shortcut-modes"><span>⌃ Control + ⌘ Command + Key</span><span>⌥ Option + ⌘ Command + Key</span><span>⇧ Shift + ⌘ Command + Key</span><span>⌃ / ⌥ / ⌃⌥ / ⌃⇧ + Key</span></div>
      </Show>
      <h2 class="shortcut-heading">全域快捷鍵</h2>
      <For each={[
        { id: "ptt" as Action, title: "PTT 聽寫", desc: "按住開始聽寫，放開結束；是否自動送出由 General 控制。" },
        { id: "submit" as Action, title: "Explicit Submit", desc: "前景送出訊息；隱藏至系統匣時切換預設專案。" },
        { id: "switchTabs" as Action, title: "Switch Tabs", desc: "依目前分頁排列切換下一個 Tab，最後一個會回到第一個。" },
      ]}>
        {action => (
          <section class="shortcut-action">
            <div><div class="shortcut-action-name">{action.title}</div><p class="shortcut-desc">{action.desc}</p></div>
            <div class="shortcut-controls">
              <For each={settings()[action.id]}>
                {(binding, index) => (
                  <div class="shortcut-binding">
                    <div class="shortcut-key-combo"><For each={format(binding)}>{(segment, i) =>
                      <><Show when={i() > 0}><span class="shortcut-plus">+</span></Show><kbd>{segment}</kbd></>
                    }</For></div>
                    <div class="shortcut-buttons">
                      <button type="button" disabled={busy()} onClick={() => start(action.id, index())}>變更</button>
                      <button type="button" class="shortcut-remove" aria-label="移除快捷鍵" title="移除" disabled={busy() || settings()[action.id].length <= 1} onClick={() => remove(action.id, index())}>×</button>
                    </div>
                  </div>
                )}
              </For>
              <button type="button" class="shortcut-add" disabled={busy() || settings()[action.id].length >= 16} onClick={() => start(action.id, null)}>＋ 新增快捷鍵</button>
            </div>
          </section>
        )}
      </For>
      <p class="shortcut-note">同一組快捷鍵不能重複指派。若{platform().os === "macos" ? " macOS 系統" : " Windows"}或其他程式已佔用組合，儲存時會顯示錯誤並還原先前的設定。</p>
      <div class="shortcut-footer">
        <button class="shortcut-reset" type="button" disabled={busy()} onClick={reset}>恢復預設</button>
        <div class="shortcut-footer-actions"><span>{changed() ? "尚未儲存" : "目前設定已同步"}</span><button class="shortcut-save" type="button" disabled={busy() || !changed()} onClick={() => void save()}>儲存變更</button></div>
      </div>
      <Show when={error()}><p role="alert" class="shortcut-error">{error()}</p></Show>
      <Show when={status()}><p role="status" class="shortcut-success">{status()}</p></Show>
      <Show when={recording()}>
        <div class="shortcut-overlay" onClick={cancel}>
          <div class="shortcut-dialog" role="dialog" aria-modal="true" aria-label="錄製快捷鍵" onClick={(event) => event.stopPropagation()}>
            <h2>設定快捷鍵</h2>
            <p>直接按下 Ctrl、Alt、Ctrl + Alt 或 Ctrl + Shift 搭配任意支援的按鍵。</p>
            <div class="shortcut-listening"><span class="shortcut-record-dot"></span>等待按鍵輸入…</div>
            <Show when={captureError()}><p role="alert" class="shortcut-error">{captureError()}</p></Show>
            <button type="button" class="shortcut-reset" onClick={cancel}>取消（Esc）</button>
          </div>
        </div>
      </Show>
    </>
  );
}
