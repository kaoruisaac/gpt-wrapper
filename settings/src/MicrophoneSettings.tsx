import { createSignal, For, Show, onMount, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { PROVIDER_CHOICES, type ProviderId, providerChoice } from "./providers";
import { SYSTEM_DEFAULT_HINT, SYSTEM_DEFAULT_LABEL } from "./platform";

type MicrophonePreference =
  | { mode: "default" }
  | { mode: "device"; deviceId: string; groupId: string; label: string };
type MicrophoneDevice = { deviceId: string; groupId: string; label: string };
type Binding = "default" | "selected" | "confirmed" | "needsConfirmation";
type MicrophoneInventory = {
  devices: MicrophoneDevice[];
  status: "ready" | "needsPermission";
  detail: string;
  provider: ProviderId;
  origin: string;
  binding: Binding;
  selectedDeviceId: string | null;
};

// One shared microphone choice. Device IDs are per website origin, so the
// list is read from the selected provider's own tab, and choosing the same
// device from the other provider confirms that provider's ID.
export default function MicrophoneSettings() {
  const [preference, setPreference] = createSignal<MicrophonePreference>({ mode: "default" });
  const [provider, setProvider] = createSignal<ProviderId>("chatgpt");
  const [inventory, setInventory] = createSignal<MicrophoneInventory | null>(null);
  const [open, setOpen] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [message, setMessage] = createSignal("");
  const [error, setError] = createSignal("");
  let menuRef: HTMLDivElement | undefined;
  let requestVersion = 0;

  const devices = () => inventory()?.devices ?? [];
  const deviceState = () => inventory()?.status ?? "unavailable";
  const name = () => providerChoice(provider()).name;
  const selectedHere = (device: MicrophoneDevice) =>
    preference().mode === "device" && inventory()?.selectedDeviceId === device.deviceId;

  const labelFor = (item: MicrophoneDevice) => {
    const same = devices().filter(d => d.label === item.label);
    const index = same.findIndex(d => d.deviceId === item.deviceId);
    return same.length > 1 ? `${item.label} · #${index + 1}` : item.label;
  };

  const bindingText = () => {
    const current = inventory();
    if (!current || preference().mode === "default") return "";
    switch (current.binding) {
      case "selected": return `${name()} 使用此處選取的裝置。`;
      case "confirmed": return `${name()} 已確認對應同一個裝置。`;
      default: return `${name()} 尚未確認此裝置：聽寫時會以裝置名稱在 ${name()} 頁面比對，若無法唯一辨識會停止並提示，請在下方清單重新選取以確認。`;
    }
  };

  const refresh = async (requestAccess = false) => {
    const version = ++requestVersion;
    const source = provider();
    setLoading(true);
    setError("");
    try {
      const next = await invoke<MicrophoneInventory>("list_microphone_inputs", {
        provider: source, requestAccess,
      });
      if (version !== requestVersion) return;
      setInventory(next);
      setMessage(next.detail);
    } catch (cause) {
      if (version !== requestVersion) return;
      setInventory(null);
      setError(String(cause));
    } finally {
      if (version === requestVersion) setLoading(false);
    }
  };

  const readPreference = async () => {
    try { setPreference(await invoke<MicrophonePreference>("get_preferred_microphone")); }
    catch (readError) { setError(`無法確認目前設定：${readError}`); }
  };

  const save = async (next: MicrophonePreference, done: string) => {
    if (saving()) return;
    setSaving(true);
    setError("");
    try {
      setPreference(await invoke<MicrophonePreference>("set_preferred_microphone", {
        provider: provider(), preference: next,
      }));
      setMessage(done);
      await refresh();
    } catch (cause) {
      setError(String(cause));
      await readPreference();
    } finally {
      setSaving(false);
      setOpen(false);
    }
  };

  const chooseDevice = (device: MicrophoneDevice) => {
    if (saving() || !device.label || deviceState() !== "ready") return;
    void save(
      { mode: "device", deviceId: device.deviceId, label: device.label, groupId: device.groupId },
      `已儲存。${name()} 的聽寫將使用所選裝置；其他平台需以同名裝置確認。實際收音仍需實機驗收。`,
    );
  };

  const chooseDefault = () =>
    void save({ mode: "default" }, `已使用 ${SYSTEM_DEFAULT_LABEL}，維持各平台聽寫的原生音訊行為。`);

  const selectProvider = (next: ProviderId) => {
    if (next === provider()) return;
    setProvider(next);
    setOpen(false);
    setInventory(null);
    setMessage("");
    void refresh();
  };

  onMount(() => {
    void readPreference();
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
    <section class="microphone-section" aria-label="聽寫麥克風">
      <div class="microphone-heading">
        <div>
          <h2>聽寫麥克風</h2>
          <p class="setting-description">ChatGPT 與 Claude 共用同一個麥克風選擇。裝置清單由所選平台的分頁列舉（各網站的裝置 ID 不同），不使用作業系統的裝置 ID 代替。</p>
        </div>
        <button type="button" class="microphone-refresh" onClick={() => void refresh()} disabled={loading()}>
          {loading() ? "查詢中…" : "重新整理"}
        </button>
      </div>
      <div class="provider-tabs" role="tablist" aria-label="裝置清單來源">
        <For each={PROVIDER_CHOICES}>{(item) => (
          <button type="button" role="tab" class="provider-tab"
            classList={{ "is-active": provider() === item.id }}
            aria-selected={provider() === item.id}
            disabled={saving()}
            onClick={() => selectProvider(item.id)}>
            <span class="provider-letter" aria-hidden="true">{item.letter}</span>{item.name}
          </button>
        )}</For>
      </div>
      <Show when={inventory()}>
        <p class="microphone-note">清單來源：{name()}（{inventory()!.origin}）</p>
      </Show>
      <div class="microphone-dropdown" ref={menuRef}>
        <button
          type="button"
          class="microphone-trigger"
          aria-haspopup="listbox"
          aria-expanded={open()}
          onClick={() => setOpen(v => !v)}
        >
          <span>{preference().mode === "default" ? SYSTEM_DEFAULT_LABEL : (preference() as { label: string }).label}</span>
          <span aria-hidden="true">⌄</span>
        </button>
        <Show when={open()}>
          <div class="microphone-menu" role="listbox" aria-label="麥克風來源">
            <button role="option" type="button" aria-selected={preference().mode === "default"}
              onClick={chooseDefault} disabled={saving()}>
              <span>{SYSTEM_DEFAULT_LABEL} <small>{SYSTEM_DEFAULT_HINT}</small></span>
              <Show when={preference().mode === "default"}><span aria-hidden="true">✓</span></Show>
            </button>
            <Show when={devices().length > 0}>
              <div class="microphone-menu-divider" />
              <For each={devices()}>{device =>
                <button role="option" type="button"
                  disabled={saving() || !device.label || deviceState() !== "ready"}
                  aria-selected={selectedHere(device)}
                  onClick={() => chooseDevice(device)}
                  title={!device.label ? `請先允許 ${name()} 的麥克風權限` : `選擇此裝置用於 ${name()} 聽寫`}>
                  <span>{device.label ? labelFor(device) : "未授權裝置"}
                    <small>{!device.label ? `請先授予 ${name()} 麥克風權限` : `${name()} 頁面的裝置`}</small>
                  </span>
                  <Show when={selectedHere(device)}><span aria-hidden="true">✓</span></Show>
                </button>
              }</For>
            </Show>
            <Show when={!devices().length}>
              <p class="microphone-empty">尚無可辨識的音訊輸入裝置。</p>
            </Show>
          </div>
        </Show>
      </div>
      <Show when={bindingText()}>
        <p class="microphone-binding"
          classList={{ "is-warning": inventory()?.binding === "needsConfirmation" }}>{bindingText()}</p>
      </Show>
      <p class="microphone-warning" role="status">
        指定裝置在受控的聽寫（與 ChatGPT Voice Mode）擷取時強制驗證 ID；失敗可直接重試，不會切回系統預設裝置或鎖定其他分頁。進行中的 Voice 如切換音源，請重新啟動 Voice。雙實體音源隔離仍待實機驗收。
      </p>
      <Show when={preference().mode === "device" && inventory()?.binding !== "needsConfirmation" &&
        devices().length > 0 && !devices().some(device => selectedHere(device))}>
        <p class="microphone-error">{name()} 清單未列出所選麥克風。可先確認連線及 {name()} 權限，再重新整理；設定不會自動更動。</p>
      </Show>
      <Show when={deviceState() === "needsPermission"}>
        <p class="microphone-note">
          裝置名稱受到 {name()} 網站權限限制。
          <button type="button" class="microphone-refresh" disabled={loading()}
            onClick={() => void refresh(true)}>允許 {name()} 麥克風</button>
        </p>
      </Show>
      <Show when={message()}><p class="microphone-note">{message()}</p></Show>
      <Show when={error()}><p class="microphone-error" role="alert">{error()}</p></Show>
    </section>
  );
}
