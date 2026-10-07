import { For, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { render } from "solid-js/web";
import { invoke } from "@tauri-apps/api/core";
import { FiPlus, FiX } from "solid-icons/fi";
import "./styles.css";

type TabInfo = {
  id: number;
  title: string;
};

type TabSnapshot = {
  tabs: TabInfo[];
  activeTabId: number | null;
};

type TabHostApi = {
  refresh: () => Promise<void>;
};

declare global {
  interface Window {
    __GPT_WRAPPER_TAB_HOST__?: TabHostApi;
  }
}

const EMPTY_SNAPSHOT: TabSnapshot = {
  tabs: [],
  activeTabId: null,
};

const LONG_PRESS_MS = 400;

function TabBarApp() {
  const [snapshot, setSnapshot] = createSignal<TabSnapshot>(EMPTY_SNAPSHOT);
  const [busy, setBusy] = createSignal(false);
  let gesture: { pointerId: number; startedAt: number; timer: ReturnType<typeof setTimeout> } | null = null;

  const refresh = async () => {
    try {
      setSnapshot(await invoke<TabSnapshot>("get_tabs"));
    } catch (error) {
      console.error("[gpt-wrapper] failed to refresh tabs:", error);
    }
  };

  const runCommand = async (
    command: "create_tab" | "create_project_tab" | "switch_tab" | "close_tab",
    args: Record<string, unknown> = {},
  ) => {
    if (busy()) return;

    setBusy(true);
    try {
      setSnapshot(await invoke<TabSnapshot>(command, args));
    } catch (error) {
      console.error(`[gpt-wrapper] ${command} failed:`, error);
    } finally {
      setBusy(false);
    }
  };

  const clearGesture = (button?: HTMLButtonElement) => {
    if (!gesture) return;
    clearTimeout(gesture.timer);
    if (button?.hasPointerCapture(gesture.pointerId)) {
      button.releasePointerCapture(gesture.pointerId);
    }
    gesture = null;
  };

  const startNewTabGesture = (event: PointerEvent) => {
    if (busy() || gesture || !event.isPrimary || event.button !== 0) return;
    event.preventDefault();
    const button = event.currentTarget as HTMLButtonElement;
    button.setPointerCapture(event.pointerId);
    const pointerId = event.pointerId;
    const timer = setTimeout(() => {
      if (!gesture || gesture.pointerId !== pointerId) return;
      // After the threshold, this gesture cannot fall back to a Default tab.
      gesture = null;
      setBusy(true);
      const rect = button.getBoundingClientRect();
      if (button.hasPointerCapture(pointerId)) button.releasePointerCapture(pointerId);
      void (async () => {
        try {
          const projectId = await invoke<string | null>("show_project_bookmark_menu", {
            anchor: { right: rect.right, bottom: rect.bottom },
          });
          if (projectId) {
            setSnapshot(await invoke<TabSnapshot>("create_project_tab", { projectId }));
          }
        } catch (error) {
          console.error("[gpt-wrapper] bookmark menu failed:", error);
        } finally {
          setBusy(false);
        }
      })();
    }, LONG_PRESS_MS);
    gesture = { pointerId, startedAt: performance.now(), timer };
  };

  const finishNewTabGesture = (event: PointerEvent) => {
    if (!gesture || gesture.pointerId !== event.pointerId) return;
    const button = event.currentTarget as HTMLButtonElement;
    const isShortPress = performance.now() - gesture.startedAt < LONG_PRESS_MS;
    clearGesture(button);
    if (event.type === "pointerup" && isShortPress) void runCommand("create_tab");
  };

  const closeTab = (event: MouseEvent | KeyboardEvent, tabId: number) => {
    event.preventDefault();
    event.stopPropagation();
    void runCommand("close_tab", { tabId });
  };

  const handleCloseKeyDown = (event: KeyboardEvent, tabId: number) => {
    if (event.key === "Enter" || event.key === " ") {
      closeTab(event, tabId);
    }
  };

  createEffect(() => {
    const activeTabId = snapshot().activeTabId;
    if (activeTabId === null) return;

    queueMicrotask(() => {
      document
        .querySelector<HTMLElement>(`[data-tab-id="${activeTabId}"]`)
        ?.scrollIntoView({ block: "nearest", inline: "nearest" });
    });
  });

  onMount(() => {
    const hostApi: TabHostApi = { refresh };
    window.__GPT_WRAPPER_TAB_HOST__ = hostApi;
    void refresh();

    onCleanup(() => {
      clearGesture();
      if (window.__GPT_WRAPPER_TAB_HOST__ === hostApi) {
        delete window.__GPT_WRAPPER_TAB_HOST__;
      }
    });
  });

  return (
    <div id="gpt-wrapper-tab-bar" classList={{ "is-busy": busy() }}>
      <div class="gpt-wrapper-tabs" role="tablist" aria-label="ChatGPT tabs">
        <For each={snapshot().tabs}>
          {(tab) => {
            const isActive = () => tab.id === snapshot().activeTabId;

            return (
              <button
                class="gpt-wrapper-tab"
                classList={{ "is-active": isActive() }}
                type="button"
                data-tab-id={tab.id}
                aria-current={isActive() ? "page" : "false"}
                title={tab.title}
                onClick={() => {
                  if (!busy() && !isActive()) {
                    void runCommand("switch_tab", { tabId: tab.id });
                  }
                }}
              >
                <span class="gpt-wrapper-tab-dot" aria-hidden="true" />
                <span class="gpt-wrapper-tab-title">{tab.title}</span>
                <span
                  class="gpt-wrapper-tab-close"
                  role="button"
                  tabIndex={0}
                  aria-label={`Close ${tab.title}`}
                  title="Close tab"
                  onClick={(event) => closeTab(event, tab.id)}
                  onKeyDown={(event) => handleCloseKeyDown(event, tab.id)}
                >
                  <FiX aria-hidden="true" />
                </span>
              </button>
            );
          }}
        </For>
      </div>

      <button
        class="gpt-wrapper-new-tab"
        type="button"
        aria-label="Open new ChatGPT tab"
        title="New ChatGPT tab"
        disabled={busy()}
        onPointerDown={startNewTabGesture}
        onPointerUp={finishNewTabGesture}
        onPointerCancel={finishNewTabGesture}
        onLostPointerCapture={finishNewTabGesture}
        onClick={(event) => {
          if (event.detail === 0) void runCommand("create_tab");
        }}
      >
        <FiPlus aria-hidden="true" />
      </button>
    </div>
  );
}

const root = document.getElementById("root");
if (!root) {
  throw new Error("Tab bar root element is missing.");
}

render(() => <TabBarApp />, root);
