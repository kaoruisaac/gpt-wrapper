(() => {
  if (window.__GPT_WRAPPER__) return;

  const state = { before: "", timer: null };
  const q = (selector) => document.querySelector(selector);
  const visible = (el) => {
    if (!el) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const buttons = () => [...document.querySelectorAll("button")];
  const aria = (b) => (b?.getAttribute("aria-label") || "").trim();
  // Observed ChatGPT button labels in Traditional Chinese and English.
  const CONTROL_LABELS = {
    stop: ["停止", "Stop"],
    send: ["傳送", "Send"],
    voice: ["開始語音對話", "Start Voice"],
  };

  const composer = () =>
    q("textarea#mobile-composer-prompt") ||
    q("textarea[name='prompt']") ||
    q("#prompt-textarea") ||
    q('[data-testid="prompt-textarea"]') ||
    q('form [contenteditable="true"]');

  const composerText = () => {
    const el = composer();
    return (el?.value ?? el?.innerText ?? el?.textContent ?? "").trim();
  };

  const sendButton = () => {
    const labels = [...CONTROL_LABELS.send, "傳送訊息", "Send message", "Send prompt"];
    return buttons().find((b) => visible(b) && labels.includes(aria(b))) ||
      q('[data-testid="send-button"]') ||
      q("#composer-submit-button");
  };

  // The composer may be rerendered, so search it first and then fall back to the page.
  const matchingAriaButton = (labels) => {
    const find = (root) =>
      [...root.querySelectorAll("button[aria-label]")].find(
        (button) => visible(button) && labels.includes(aria(button))
      );
    const form = composer()?.closest("form");
    return (form && find(form)) || find(document);
  };

  const startStatusObserver = () => {
    let lastStatus = null;
    let pending = null;
    const update = () => {
      const status = matchingAriaButton(CONTROL_LABELS.stop) ? "working" :
        matchingAriaButton(CONTROL_LABELS.send) ? "send" :
        matchingAriaButton(CONTROL_LABELS.voice) ? "voice" : "fallback";
      if (status === lastStatus) return;
      const webview = window.chrome?.webview;
      if (!webview?.postMessage) return;
      try {
        webview.postMessage(JSON.stringify({ kind: "gpt-wrapper-tab-status", status }));
        lastStatus = status;
      } catch (error) {
        console.debug("[gpt-wrapper] tab status delivery failed", error);
      }
    };
    // Rust requests a fresh snapshot after navigation in case the first message was missed.
    window.__GPT_WRAPPER__.reportStatus = () => {
      lastStatus = null;
      update();
    };
    const scheduleUpdate = () => {
      if (pending !== null) return;
      pending = setTimeout(() => {
        pending = null;
        update();
      }, 100);
    };
    const observer = new MutationObserver(scheduleUpdate);
    observer.observe(document.documentElement, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: ["aria-label", "class", "style", "hidden", "disabled", "aria-disabled"],
    });
    update();
    window.addEventListener("pagehide", () => {
      observer.disconnect();
      if (pending !== null) clearTimeout(pending);
    }, { once: true });
  };

  const focusComposerAtEnd = (el) => {
    el.focus({ preventScroll: true });

    if (el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement) {
      const end = el.value.length;
      el.setSelectionRange(end, end);
      return (
        document.activeElement === el &&
        el.selectionStart === end &&
        el.selectionEnd === end
      );
    }

    if (el.isContentEditable) {
      const selection = window.getSelection();
      if (!selection) return false;
      const range = document.createRange();
      range.selectNodeContents(el);
      range.collapse(false);
      selection.removeAllRanges();
      selection.addRange(range);
      return (
        document.activeElement === el &&
        selection.rangeCount === 1 &&
        selection.getRangeAt(0).collapsed &&
        el.contains(selection.getRangeAt(0).endContainer)
      );
    }

    return false;
  };

  window.__GPT_WRAPPER__ = {
    applyMessagePrefix(prefix) {
      if (prefix === "") return "success";
      const el = composer();
      if (!el || !(el instanceof HTMLElement) || !el.isConnected) return "not-ready";

      try {
        if (el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement) {
          if (el.disabled || el.readOnly) return "not-ready";
          const prototype = el instanceof HTMLTextAreaElement
            ? HTMLTextAreaElement.prototype
            : HTMLInputElement.prototype;
          Object.getOwnPropertyDescriptor(prototype, "value").set.call(el, prefix);
        } else if (el.isContentEditable) {
          el.textContent = prefix;
        } else {
          return "not-ready";
        }
        el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: prefix }));
        const actual = el.value ?? el.innerText ?? el.textContent ?? "";
        if (actual !== prefix) return "failure";
        return focusComposerAtEnd(el) ? "success" : "failure";
      } catch (error) {
        console.error("[gpt-wrapper] message prefix injection failed", error);
        return "failure";
      }
    },

    prepareStart() {
      clearInterval(state.timer);
      state.before = composerText();
    },

    waitAndSend() {
      clearInterval(state.timer);
      const started = Date.now();

      state.timer = setInterval(() => {
        const current = composerText();
        const send = sendButton();
        const changed = current.length > 0 && current !== state.before;
        const enabled =
          !!send &&
          !send.disabled &&
          send.getAttribute("aria-disabled") !== "true";

        if (changed && enabled) {
          clearInterval(state.timer);
          send.click();
          return;
        }

        if (Date.now() - started > 15000) {
          clearInterval(state.timer);
          console.warn("[gpt-wrapper] Timed out waiting for transcription.");
        }
      }, 250);
    },
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", startStatusObserver, { once: true });
  } else {
    startStatusObserver();
  }
})();
