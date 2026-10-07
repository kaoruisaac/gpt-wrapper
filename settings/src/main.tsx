import { createSignal, onMount } from "solid-js";
import { render } from "solid-js/web";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

type SettingsTab = "general" | "bookmarks";
type StatusKind = "" | "success" | "error";

function GeneralSettings() {
  const [autostartEnabled, setAutostartEnabled] = createSignal(false);
  const [busy, setBusy] = createSignal(true);
  const [statusMessage, setStatusMessage] = createSignal("");
  const [statusKind, setStatusKind] = createSignal<StatusKind>("");

  const setStatus = (message = "", kind: StatusKind = "") => {
    setStatusMessage(message);
    setStatusKind(kind);
  };

  const readAutostartState = async () => {
    setBusy(true);
    try {
      const enabled = Boolean(await invoke<boolean>("get_autostart_enabled"));
      setAutostartEnabled(enabled);
      setStatus();
      return enabled;
    } catch (error) {
      console.error("[gpt-wrapper] failed to read autostart state:", error);
      setStatus("無法讀取開機自動啟動狀態。", "error");
      throw error;
    } finally {
      setBusy(false);
    }
  };

  const handleAutostartChange = async (event: Event & { currentTarget: HTMLInputElement }) => {
    if (busy()) return;

    const previous = autostartEnabled();
    const desired = event.currentTarget.checked;

    setAutostartEnabled(desired);
    setBusy(true);
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
      setBusy(false);
    }
  };

  onMount(() => {
    void readAutostartState().catch(() => {});
  });

  return (
    <>
      <h1>General</h1>

      <div class="setting-row">
        <input
          id="autostart"
          type="checkbox"
          checked={autostartEnabled()}
          disabled={busy()}
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

function ProjectBookmarksPlaceholder() {
  return (
    <>
      <h1>專案書籤</h1>
      <p class="placeholder">專案書籤功能將於後續加入。</p>
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
          <ProjectBookmarksPlaceholder />
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
