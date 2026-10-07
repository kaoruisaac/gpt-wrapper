(() => {
  if (window.__GPT_WRAPPER_TAB_HOST__) return;

  const invoke = (command, args = {}) => {
    const internal = window.__TAURI_INTERNALS__;
    if (!internal?.invoke) {
      throw new Error("Tauri IPC is unavailable in the tab host.");
    }
    return internal.invoke(command, args);
  };

  let snapshot = { tabs: [], activeTabId: null };
  let busy = false;

  const escapeHtml = (value) =>
    String(value)
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;")
      .replaceAll("'", "&#039;");

  const render = () => {
    const tabsRoot = document.querySelector(".gpt-wrapper-tabs");
    if (!tabsRoot) return;

    tabsRoot.innerHTML = snapshot.tabs
      .map((tab) => {
        const active = tab.id === snapshot.activeTabId;
        return `
          <button
            class="gpt-wrapper-tab${active ? " is-active" : ""}"
            type="button"
            data-tab-id="${tab.id}"
            aria-current="${active ? "page" : "false"}"
            title="${escapeHtml(tab.title)}"
          >
            <span class="gpt-wrapper-tab-dot" aria-hidden="true"></span>
            <span class="gpt-wrapper-tab-title">${escapeHtml(tab.title)}</span>
            <span
              class="gpt-wrapper-tab-close"
              role="button"
              tabindex="0"
              aria-label="Close ${escapeHtml(tab.title)}"
              title="Close tab"
            >×</span>
          </button>
        `;
      })
      .join("");

    tabsRoot.querySelectorAll(".gpt-wrapper-tab").forEach((tabButton) => {
      const tabId = Number(tabButton.dataset.tabId);

      tabButton.addEventListener("click", async () => {
        if (busy || tabId === snapshot.activeTabId) return;
        await runCommand("switch_tab", { tabId });
      });

      const close = tabButton.querySelector(".gpt-wrapper-tab-close");
      const closeTab = async (event) => {
        event.preventDefault();
        event.stopPropagation();
        if (busy) return;
        await runCommand("close_tab", { tabId });
      };

      close?.addEventListener("click", closeTab);
      close?.addEventListener("keydown", async (event) => {
        if (event.key === "Enter" || event.key === " ") {
          await closeTab(event);
        }
      });
    });

    document
      .querySelector(`[data-tab-id="${snapshot.activeTabId}"]`)
      ?.scrollIntoView({ block: "nearest", inline: "nearest" });
  };

  const runCommand = async (command, args = {}) => {
    busy = true;
    document.body.classList.add("is-busy");
    try {
      snapshot = await invoke(command, args);
      render();
    } catch (error) {
      console.error(`[gpt-wrapper] ${command} failed:`, error);
    } finally {
      busy = false;
      document.body.classList.remove("is-busy");
    }
  };

  const refresh = async () => {
    try {
      snapshot = await invoke("get_tabs");
      render();
    } catch (error) {
      console.error("[gpt-wrapper] failed to refresh tabs:", error);
    }
  };

  window.__GPT_WRAPPER_TAB_HOST__ = { refresh };

  const mount = async () => {
    if (!document.head || !document.body || document.getElementById("gpt-wrapper-tab-bar")) {
      return;
    }

    const style = document.createElement("style");
    style.id = "gpt-wrapper-tab-bar-style";
    style.textContent = `
      :root {
        color-scheme: light;
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
        background: #f4f4f4;
      }

      html,
      body {
        width: 100%;
        height: 100%;
        margin: 0;
        overflow: hidden;
        background: #f4f4f4;
      }

      body.is-busy {
        cursor: progress;
      }

      #gpt-wrapper-tab-bar {
        width: 100%;
        height: 38px;
        display: flex;
        align-items: end;
        gap: 6px;
        padding: 5px 8px 0;
        box-sizing: border-box;
        background: #f4f4f4;
        border-bottom: 1px solid rgba(0, 0, 0, 0.10);
        user-select: none;
      }

      .gpt-wrapper-tabs {
        min-width: 0;
        height: 33px;
        flex: 1 1 auto;
        display: flex;
        align-items: end;
        gap: 4px;
        overflow-x: auto;
        overflow-y: hidden;
        scrollbar-width: none;
      }

      .gpt-wrapper-tabs::-webkit-scrollbar {
        display: none;
      }

      .gpt-wrapper-tab {
        height: 31px;
        flex: 0 1 220px;
        min-width: 132px;
        max-width: 240px;
        padding: 0 8px 0 12px;
        border: 0;
        border-radius: 9px 9px 0 0;
        display: flex;
        align-items: center;
        gap: 8px;
        background: #e9e9e9;
        color: #565869;
        font: inherit;
        font-size: 12px;
        font-weight: 500;
        cursor: pointer;
        transition: background 120ms ease, color 120ms ease;
      }

      .gpt-wrapper-tab:hover {
        background: #efefef;
        color: #202123;
      }

      .gpt-wrapper-tab.is-active {
        background: #ffffff;
        color: #202123;
        box-shadow:
          0 -1px 0 rgba(0, 0, 0, 0.04),
          1px 0 0 rgba(0, 0, 0, 0.06),
          -1px 0 0 rgba(0, 0, 0, 0.06);
      }

      .gpt-wrapper-tab-dot {
        width: 8px;
        height: 8px;
        flex: 0 0 auto;
        border-radius: 999px;
        background: #10a37f;
      }

      .gpt-wrapper-tab-title {
        min-width: 0;
        flex: 1 1 auto;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        text-align: left;
      }

      .gpt-wrapper-tab-close {
        width: 20px;
        height: 20px;
        flex: 0 0 auto;
        border-radius: 6px;
        display: grid;
        place-items: center;
        color: #6e6e80;
        font-size: 16px;
        line-height: 1;
      }

      .gpt-wrapper-tab-close:hover,
      .gpt-wrapper-tab-close:focus-visible {
        background: rgba(0, 0, 0, 0.08);
        color: #202123;
        outline: none;
      }

      .gpt-wrapper-new-tab {
        width: 29px;
        height: 29px;
        flex: 0 0 29px;
        margin-bottom: 2px;
        border: 0;
        border-radius: 8px;
        display: grid;
        place-items: center;
        background: transparent;
        color: #565869;
        cursor: pointer;
        transition: background 120ms ease, color 120ms ease;
      }

      .gpt-wrapper-new-tab:hover {
        background: rgba(0, 0, 0, 0.07);
        color: #202123;
      }

      .gpt-wrapper-new-tab:active {
        background: rgba(0, 0, 0, 0.11);
      }

      .gpt-wrapper-new-tab svg {
        width: 16px;
        height: 16px;
        pointer-events: none;
      }
    `;

    const bar = document.createElement("div");
    bar.id = "gpt-wrapper-tab-bar";
    bar.innerHTML = `
      <div class="gpt-wrapper-tabs" role="tablist" aria-label="ChatGPT tabs"></div>
      <button
        class="gpt-wrapper-new-tab"
        type="button"
        aria-label="Open new ChatGPT tab"
        title="New ChatGPT tab"
      >
        <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path
            d="M12 5v14M5 12h14"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
          />
        </svg>
      </button>
    `;

    bar.querySelector(".gpt-wrapper-new-tab")?.addEventListener("click", async () => {
      if (busy) return;
      await runCommand("create_tab");
    });

    document.head.appendChild(style);
    document.body.appendChild(bar);

    await refresh();
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount, { once: true });
  } else {
    mount();
  }
})();
