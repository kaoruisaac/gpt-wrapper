import { createSignal, onMount } from "solid-js";
import { render } from "solid-js/web";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";
import ProjectBookmarks from "./ProjectBookmarks";
import MicrophoneSettings from "./MicrophoneSettings";
import ShortcutsSettings from "./ShortcutsSettings";
import DevicesSettings from "./DevicesSettings";

type SettingsTab = "general" | "devices" | "bookmarks" | "shortcuts";
type StatusKind = "" | "success" | "error";
function GeneralSettings() {
  const [autostartEnabled, setAutostartEnabled] = createSignal(false);
  const [autostartBusy, setAutostartBusy] = createSignal(true);
  const [pttAutoSubmitEnabled, setPttAutoSubmitEnabled] = createSignal(true);
  const [pttBusy, setPttBusy] = createSignal(true);
  const [notificationSoundEnabled, setNotificationSoundEnabled] = createSignal(true);
  const [notificationSoundBusy, setNotificationSoundBusy] = createSignal(true);
  const [statusMessage, setStatusMessage] = createSignal("");
  const [statusKind, setStatusKind] = createSignal<StatusKind>("");
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

  const readNotificationSoundState = async () => {
    setNotificationSoundBusy(true);
    try {
      setNotificationSoundEnabled(await invoke<boolean>("get_notification_sound_enabled"));
    } catch (error) {
      console.error("[gpt-wrapper] failed to read notification sound state:", error);
      setStatus("無法讀取通知音效設定。", "error");
    } finally {
      setNotificationSoundBusy(false);
    }
  };

  const handleNotificationSoundChange = async (event: Event & { currentTarget: HTMLInputElement }) => {
    if (notificationSoundBusy()) return;
    const previous = notificationSoundEnabled();
    const desired = event.currentTarget.checked;
    setNotificationSoundEnabled(desired);
    setNotificationSoundBusy(true);
    setStatus();
    try {
      const actual = await invoke<boolean>("set_notification_sound_enabled", { enabled: desired });
      setNotificationSoundEnabled(actual);
      setStatus(
        actual === desired
          ? desired ? "已啟用通知音效。" : "已停用通知音效。"
          : "系統回報的設定狀態與要求不同，已重新同步。",
        actual === desired ? "success" : "error",
      );
    } catch (error) {
      console.error("[gpt-wrapper] failed to update notification sound:", error);
      try {
        setNotificationSoundEnabled(await invoke<boolean>("get_notification_sound_enabled"));
      } catch (syncError) {
        console.error("[gpt-wrapper] failed to resync notification sound:", syncError);
        setNotificationSoundEnabled(previous);
      }
      setStatus("更新通知音效失敗，已恢復實際設定狀態。", "error");
    } finally {
      setNotificationSoundBusy(false);
    }
  };

  onMount(() => {
    void readAutostartState().catch(() => {});
    void readPttAutoSubmitState();
    void readNotificationSoundState();
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

      <div class="setting-row">
        <input
          id="notification-sound"
          type="checkbox"
          checked={notificationSoundEnabled()}
          disabled={notificationSoundBusy()}
          onChange={handleNotificationSoundChange}
        />
        <div class="setting-copy">
          <label class="setting-label" for="notification-sound">通知音效</label>
          <p class="setting-description">ChatGPT 回覆完成且顯示背景通知時播放提示音</p>
        </div>
      </div>

      <MicrophoneSettings />

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
          aria-selected={activeTab() === "devices"}
          onClick={() => setActiveTab("devices")}
        >
          Devices
        </button>
        <button
          class="tab-button"
          type="button"
          aria-selected={activeTab() === "bookmarks"}
          onClick={() => setActiveTab("bookmarks")}
        >
          專案書籤
        </button>
        <button
          class="tab-button"
          type="button"
          aria-selected={activeTab() === "shortcuts"}
          onClick={() => setActiveTab("shortcuts")}
        >
          Shortcuts
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
          class="panel devices-panel"
          classList={{ "is-active": activeTab() === "devices" }}
          aria-hidden={activeTab() !== "devices"}
        >
          <DevicesSettings />
        </section>

        <section
          class="panel"
          classList={{ "is-active": activeTab() === "bookmarks" }}
          aria-hidden={activeTab() !== "bookmarks"}
        >
          <ProjectBookmarks />
        </section>

        <section
          class="panel"
          classList={{ "is-active": activeTab() === "shortcuts" }}
          aria-hidden={activeTab() !== "shortcuts"}
        >
          <ShortcutsSettings />
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
