import { createSignal, For, Show, onMount, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

type MicrophonePreference =
  | { mode: "default" }
  | { mode: "device"; deviceId: string; groupId: string; label: string };
type MicrophoneDevice = { deviceId: string; groupId: string; label: string };
type MicrophoneInventory = {
  devices: MicrophoneDevice[];
  status: "ready" | "needsPermission";
  detail: string;
};

// Device selection comes from the ChatGPT WebView; only physical audio UAT is pending.
export default function MicrophoneSettings() {
  const [preference, setPreference] = createSignal<MicrophonePreference>({ mode: "default" });
  const [devices, setDevices] = createSignal<MicrophoneDevice[]>([]);
  const [open, setOpen] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [message, setMessage] = createSignal("");
  const [error, setError] = createSignal("");
  const [deviceState, setDeviceState] = createSignal<"ready" | "needsPermission" | "unavailable">("unavailable");
  let menuRef: HTMLDivElement | undefined;

  const labelFor = (item: MicrophoneDevice) => {
    const sameName = devices().filter(d => d.label === item.label).length;
    const index = devices().filter(d => d.label === item.label).findIndex(d => d.deviceId === item.deviceId);
    return sameName > 1 ? `${item.label} · #${index + 1}` : item.label;
  };

  const refresh = async () => {
    setLoading(true);
    setError("");
    try {
      const inventory = await invoke<MicrophoneInventory>("list_microphone_inputs");
      setDevices(inventory.devices);
      setDeviceState(inventory.status);
      setMessage(inventory.detail);
    } catch (cause) {
      setDevices([]);
      setDeviceState("unavailable");
      setError(String(cause));
    } finally {
      setLoading(false);
    }
  };

  const chooseDevice = async (device: MicrophoneDevice) => {
    if (saving() || !device.label || deviceState() !== "ready") return;
    setSaving(true);
    setError("");
    try {
      const actual = await invoke<MicrophonePreference>("set_preferred_microphone", {
        preference: { mode: "device", deviceId: device.deviceId, label: device.label, groupId: device.groupId },
      });
      setPreference(actual);
      setMessage("已儲存。接下來的 ChatGPT 聽寫及新啟動的 Voice Mode 將使用所選裝置；實際收音仍需雙音源驗收。");
    } catch (cause) {
      setError(String(cause));
      try { setPreference(await invoke<MicrophonePreference>("get_preferred_microphone")); }
      catch (readError) { setError(`儲存失敗，無法確認目前設定：${readError}`); }
    } finally {
      setSaving(false);
      setOpen(false);
    }
  };

  const chooseDefault = async () => {
    if (saving()) return;
    setSaving(true);
    setError("");
    try {
      const actual = await invoke<MicrophonePreference>("set_preferred_microphone", {
        preference: { mode: "default" },
      });
      setPreference(actual);
      setMessage("已使用 Windows Default，維持 ChatGPT 聽寫及 Voice Mode 的原生音訊行為。");
    } catch (cause) {
      setError(String(cause));
      try { setPreference(await invoke<MicrophonePreference>("get_preferred_microphone")); }
      catch (readError) { setError(`儲存失敗，無法確認目前設定：${readError}`); }
    } finally {
      setSaving(false);
      setOpen(false);
    }
  };

  onMount(() => {
    void invoke<MicrophonePreference>("get_preferred_microphone")
      .then(setPreference)
      .catch(cause => setError(`無法讀取麥克風設定：${cause}`));
    void refresh();
    const clickAway = (event: MouseEvent) => {
      if (!menuRef?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", clickAway);
    const esc = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("keydown", esc);
    onCleanup(() => {
      document.removeEventListener("mousedown", clickAway);
      document.removeEventListener("keydown", esc);
    });
  });

  return (
    <section class="microphone-section" aria-label="ChatGPT 麥克風">
      <div class="microphone-heading">
        <div>
          <h2>ChatGPT 麥克風</h2>
          <p class="setting-description">聽寫與 Voice Mode 共用此麥克風來源。裝置由 ChatGPT 分頁列舉，不使用 Windows 裝置 ID 代替。</p>
        </div>
        <button type="button" class="microphone-refresh" onClick={() => void refresh()} disabled={loading()}>
          {loading() ? "查詢中…" : "重新整理"}
        </button>
      </div>
      <div class="microphone-dropdown" ref={menuRef}>
        <button
          type="button"
          class="microphone-trigger"
          aria-haspopup="listbox"
          aria-expanded={open()}
          onClick={() => setOpen(v => !v)}
        >
          <span>{preference().mode === "default" ? "Windows Default" : (preference() as { label: string }).label}</span>
          <span aria-hidden="true">⌄</span>
        </button>
        <Show when={open()}>
          <div class="microphone-menu" role="listbox" aria-label="麥克風來源">
            <button role="option" type="button" aria-selected={preference().mode === "default"}
              onClick={() => void chooseDefault()} disabled={saving()}>
              <span>Windows Default <small>依 Windows 系統預設裝置</small></span>
              <Show when={preference().mode === "default"}><span aria-hidden="true">✓</span></Show>
            </button>
            <Show when={devices().length > 0}>
              <div class="microphone-menu-divider" />
              <For each={devices()}>{device =>
                <button role="option" type="button"
                  disabled={saving() || !device.label || deviceState() !== "ready"}
                  aria-selected={preference().mode === "device" && (preference() as { deviceId: string }).deviceId === device.deviceId}
                  onClick={() => void chooseDevice(device)}
                  title={!device.label ? "請先允許 ChatGPT 的麥克風權限" : "選擇此裝置用於 ChatGPT 聽寫及 Voice Mode"}>
                  <span>{device.label ? labelFor(device) : "未授權裝置"}
                    <small>{!device.label ? "請先授予 ChatGPT 麥克風權限" : "用於 ChatGPT 聽寫與 Voice Mode"}</small>
                  </span>
                  <Show when={preference().mode === "device" && (preference() as { deviceId: string }).deviceId === device.deviceId}><span aria-hidden="true">✓</span></Show>
                </button>
              }</For>
            </Show>
            <Show when={!devices().length}>
              <p class="microphone-empty">尚無可辨識的音訊輸入裝置。</p>
            </Show>
          </div>
        </Show>
      </div>
      <p class="microphone-warning" role="status">
        指定裝置在受控的聽寫與 Voice Mode 擷取時強制驗證 ID；失敗可直接重試，不會切回 Windows Default 或鎖定其他分頁。進行中的 Voice 如切換音源，請重新啟動 Voice。雙實體音源隔離仍待實機驗收。
      </p>
      <Show when={preference().mode === "device" && devices().length > 0 &&
        !devices().some(device => device.deviceId === (preference() as { deviceId: string }).deviceId &&
            device.label === (preference() as { label: string }).label)}>
        <p class="microphone-error">目前清單未列出所選麥克風。可先確認連線及 ChatGPT 權限，再重新整理；設定不會自動更動。</p>
      </Show>
      <Show when={deviceState() === "needsPermission"}>
        <p class="microphone-note">裝置名稱受到權限限制。請在 ChatGPT 頁面允許麥克風權限後重新整理。</p>
      </Show>
      <Show when={message()}><p class="microphone-note">{message()}</p></Show>
      <Show when={error()}><p class="microphone-error" role="alert">{error()}</p></Show>
    </section>
  );
}
