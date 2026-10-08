import { createSignal, onCleanup, onMount, For, Show } from "solid-js";
import { render } from "solid-js/web";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";
import ProjectBookmarks from "./ProjectBookmarks";

type SettingsTab = "general" | "bookmarks";
type StatusKind = "" | "success" | "error";
type BleDevice = { id: string; name: string };
type BleSnapshot = { enabled: boolean; approvedDevice: BleDevice | null; candidates: BleDevice[]; state: string; detail: string; lastRx: string | null };

function GeneralSettings() {
  const [autostartEnabled, setAutostartEnabled] = createSignal(false);
  const [autostartBusy, setAutostartBusy] = createSignal(true);
  const [pttAutoSubmitEnabled, setPttAutoSubmitEnabled] = createSignal(true);
  const [pttBusy, setPttBusy] = createSignal(true);
  const [statusMessage, setStatusMessage] = createSignal("");
  const [statusKind, setStatusKind] = createSignal<StatusKind>("");
  const [ble, setBle] = createSignal<BleSnapshot | null>(null);
  const [bleBusy, setBleBusy] = createSignal(false);
  const [bleError, setBleError] = createSignal("");

  const refreshBle = async () => {
    try { setBle(await invoke<BleSnapshot>("ble_status")); }
    catch (error) { setBleError(String(error)); }
  };
  const bleAction = async (command: string, args?: Record<string, unknown>) => {
    setBleBusy(true);
    setBleError("");
    try { await invoke(command, args); await refreshBle(); }
    catch (error) { setBleError(String(error)); }
    finally { setBleBusy(false); }
  };

  const setStatus = (message = "", kind: StatusKind = "") => {
    setStatusMessage(message);
    setStatusKind(kind);
  };

  const readAutostartState = async () => {
    setAutostartBusy(true);
    try {
      const enabled = Boolean(await invoke<boolean>("get_autostart_enabled"));
      setAutostartEnabled(enabled);
      return enabled;
    } catch (error) {
      console.error("[gpt-wrapper] failed to read autostart state:", error);
      setStatus("無法讀取開機自動啟動狀態。", "error");
      throw error;
    } finally {
      setAutostartBusy(false);
    }
  };

  const handleAutostartChange = async (event: Event & { currentTarget: HTMLInputElement }) => {
    if (autostartBusy()) return;

    const previous = autostartEnabled();
    const desired = event.currentTarget.checked;

    setAutostartEnabled(desired);
    setAutostartBusy(true);
    setStatus();

    try {
      const actual = Boolean(
        await invoke<boolean>("set_autostart_enabled", { enabled: desired }),
      );
      setAutostartEnabled(actual);

      if (actual !== desired) {
        setStatus("系統回報的設定狀態與要求不同，已重新同步。", "error");
      } else {
        setStatus(
          desired ? "已啟用開機自動啟動。" : "已停用開機自動啟動。",
          "success",
        );
      }
    } catch (error) {
      console.error("[gpt-wrapper] failed to update autostart:", error);

      try {
        const actual = Boolean(await invoke<boolean>("get_autostart_enabled"));
        setAutostartEnabled(actual);
      } catch (syncError) {
        console.error("[gpt-wrapper] failed to resync autostart state:", syncError);
        setAutostartEnabled(previous);
      }

      setStatus("更新失敗，已恢復實際設定狀態。", "error");
    } finally {
      setAutostartBusy(false);
    }
  };

  const readPttAutoSubmitState = async () => {
    setPttBusy(true);
    try {
      setPttAutoSubmitEnabled(await invoke<boolean>("get_ptt_auto_submit_enabled"));
    } catch (error) {
      console.error("[gpt-wrapper] failed to read PTT auto submit state:", error);
      setStatus("無法讀取 PTT shortcuts 自動送出設定。", "error");
    } finally {
      setPttBusy(false);
    }
  };

  const handlePttAutoSubmitChange = async (event: Event & { currentTarget: HTMLInputElement }) => {
    if (pttBusy()) return;
    const previous = pttAutoSubmitEnabled();
    const desired = event.currentTarget.checked;
    setPttAutoSubmitEnabled(desired);
    setPttBusy(true);
    setStatus();
    try {
      const actual = await invoke<boolean>("set_ptt_auto_submit_enabled", { enabled: desired });
      setPttAutoSubmitEnabled(actual);
      setStatus(
        actual === desired
          ? desired ? "已啟用 PTT 自動送出。" : "已停用 PTT 自動送出。"
          : "系統回報的設定狀態與要求不同，已重新同步。",
        actual === desired ? "success" : "error",
      );
    } catch (error) {
      console.error("[gpt-wrapper] failed to update PTT auto submit:", error);
      try {
        setPttAutoSubmitEnabled(await invoke<boolean>("get_ptt_auto_submit_enabled"));
      } catch (syncError) {
        console.error("[gpt-wrapper] failed to resync PTT auto submit:", syncError);
        setPttAutoSubmitEnabled(previous);
      }
      setStatus("更新 PTT 自動送出失敗，已恢復實際設定狀態。", "error");
    } finally {
      setPttBusy(false);
    }
  };

  onMount(() => {
    void readAutostartState().catch(() => {});
    void readPttAutoSubmitState();
    void refreshBle();
    const timer = window.setInterval(() => { void refreshBle(); }, 1000);
    onCleanup(() => window.clearInterval(timer));
  });

  return (
    <>
      <h1>General</h1>

      <div class="setting-row">
        <input
          id="autostart"
          type="checkbox"
          checked={autostartEnabled()}
          disabled={autostartBusy()}
          onChange={handleAutostartChange}
        />
        <div class="setting-copy">
          <label class="setting-label" for="autostart">
            開機時自動啟動 GPT Wrapper
          </label>
          <p class="setting-description">
            Windows 登入後會在背景啟動並常駐 system tray，不會自動顯示主視窗。
          </p>
        </div>
      </div>

      <div class="setting-row">
        <input
          id="ptt-auto-submit"
          type="checkbox"
          checked={pttAutoSubmitEnabled()}
          disabled={pttBusy()}
          onChange={handlePttAutoSubmitChange}
        />
        <div class="setting-copy">
          <label class="setting-label" for="ptt-auto-submit">
            PTT shortcuts 自動送出訊息
          </label>
          <p class="setting-description">
            關閉後，放開 PTT 仍會完成轉錄，但文字會保留在輸入框中，需手動送出。
          </p>
        </div>
      </div>

      <section class="ble-section" aria-label="Bluetooth Controller">
        <h2>Bluetooth Controller</h2>
        <div class="setting-row">
          <input id="ble-enabled" type="checkbox" checked={ble()?.enabled ?? false} disabled={bleBusy() || !ble()} onChange={(event) => void bleAction("ble_enable", { enabled: event.currentTarget.checked })} />
          <div class="setting-copy"><label class="setting-label" for="ble-enabled">啟用 BLE 控制器</label><p class="setting-description">使用已核准的 Android PTT 裝置控制 Windows 麥克風聽寫。</p></div>
        </div>
        <p class="ble-state"><strong>狀態：</strong>{({ scanning: "Scanning", connecting: "Connecting", "link-connected": "Link connected", "service-found": "Service found", ready: "Subscribed/Ready", disconnected: "Disconnected", error: "Error" } as Record<string, string>)[ble()?.state ?? ""] ?? "Loading"} · {ble()?.detail}</p>
        <Show when={ble()?.approvedDevice}><p class="ble-device">已核准：{ble()?.approvedDevice?.name || "未命名裝置"}</p></Show>
        <div class="ble-actions">
          <button type="button" disabled={bleBusy()} onClick={() => void bleAction("ble_scan")}>Scan</button>
          <button type="button" disabled={bleBusy() || !ble()?.approvedDevice} onClick={() => void bleAction("ble_disconnect")}>Disconnect</button>
          <button type="button" disabled={bleBusy() || !ble()?.approvedDevice} onClick={() => void bleAction("ble_forget")}>Forget</button>
        </div>
        <Show when={(ble()?.candidates.length ?? 0) > 0}>
          <div class="ble-candidates"><For each={ble()?.candidates ?? []}>{(device) => <div class="ble-candidate"><span>{device.name || "未命名裝置"} <small>{device.id}</small></span><button type="button" disabled={bleBusy()} onClick={() => void bleAction("ble_approve", { id: device.id })}>Approve/Connect</button></div>}</For></div>
        </Show>
        <Show when={ble()?.lastRx}><p class="setting-description">RX: {ble()?.lastRx}</p></Show>
        <Show when={bleError()}><p class="status is-error" role="alert">{bleError()}</p></Show>
      </section>

      <div
        class="status"
        classList={{
          "is-error": statusKind() === "error",
          "is-success": statusKind() === "success",
        }}
        role="status"
        aria-live="polite"
      >
        {statusMessage()}
      </div>
    </>
  );
}

function SettingsApp() {
  const [activeTab, setActiveTab] = createSignal<SettingsTab>("general");

  return (
    <div class="settings-shell">
      <nav class="sidebar" aria-label="Settings sections">
        <div class="sidebar-title">Settings</div>
        <button
          class="tab-button"
          type="button"
          aria-selected={activeTab() === "general"}
          onClick={() => setActiveTab("general")}
        >
          General
        </button>
        <button
          class="tab-button"
          type="button"
          aria-selected={activeTab() === "bookmarks"}
          onClick={() => setActiveTab("bookmarks")}
        >
          專案書籤
        </button>
      </nav>

      <main class="content">
        <section
          class="panel"
          classList={{ "is-active": activeTab() === "general" }}
          aria-hidden={activeTab() !== "general"}
        >
          <GeneralSettings />
        </section>

        <section
          class="panel"
          classList={{ "is-active": activeTab() === "bookmarks" }}
          aria-hidden={activeTab() !== "bookmarks"}
        >
          <ProjectBookmarks />
        </section>
      </main>
    </div>
  );
}

const root = document.getElementById("root");
if (!root) {
  throw new Error("Settings root element is missing.");
}

render(() => <SettingsApp />, root);
