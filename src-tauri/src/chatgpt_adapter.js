(() => {
  if (window.__GPT_WRAPPER__) return;

  const state = {
    before: "", timer: null, pendingTranscription: false,
    sendRequested: false, lastClickedText: null, generation: 0,
    activeSegment: false, segments: [], segmentVersion: 0, sendClicked: false,
  };
  const cancelTimer = () => {
    if (state.timer !== null) clearInterval(state.timer);
    state.timer = null;
    state.generation++;
  };
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

  const clearSubmittedDraftGuardIfChanged = () => {
    if (!state.sendClicked || state.activeSegment || composerText() === state.lastClickedText) return;
    state.sendClicked = false;
    state.sendRequested = false;
    state.lastClickedText = null;
  };
  // A Send usually clears the composer. Remember that transition so another
  // intentionally typed draft with identical text can also be submitted later.
  document.addEventListener("input", clearSubmittedDraftGuardIfChanged, true);

  // An absent/unknown completion signal is not evidence of a final transcript.
  const transcriptionBusy = () => {
    const indicator = q('[data-testid="transcription-status"]');
    if (!indicator) return null;
    if (indicator.getAttribute("aria-busy") === "true") return true;
    if (indicator.getAttribute("aria-busy") === "false") return false;
    return null;
  };

  // Send is a DOM action, never an Enter keypress. Prefer the observed
  // Traditional Chinese / English accessible names over unstable test IDs.
  const sendButton = () =>
    matchingAriaButton(CONTROL_LABELS.send) ||
    matchingAriaButton(["傳送訊息", "Send message", "Send prompt"]);

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
      if (state.timer !== null && state.sendRequested) {
        console.warn("[gpt-wrapper] Submit cancelled: page unloaded");
      }
      cancelTimer();
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
      if (state.sendClicked) {
        state.sendRequested = false;
        state.sendClicked = false;
        state.lastClickedText = null;
      }
      // A queued BLE Submit belongs to the whole draft, including later segments.
      if (!state.sendRequested) cancelTimer();
      state.before = composerText();
      state.activeSegment = true;
      state.segmentVersion++;
      if (!state.sendRequested) state.lastClickedText = null;
    },

    finishBleSegment() {
      state.activeSegment = false;
      state.segments.push({
        before: state.before,
        finishedAt: Date.now(),
        busySeen: transcriptionBusy() === true,
        settled: false,
      });
      state.pendingTranscription = true;
      state.segmentVersion++;
      console.info("[gpt-wrapper] dictation Finish processed; awaiting transcript readiness");
    },

    finishKeyboardSegment() {
      state.activeSegment = false;
    },

    cancelExplicitSend() {
      cancelTimer();
      state.pendingTranscription = false;
      state.sendRequested = false;
      state.sendClicked = false;
      state.activeSegment = false;
      state.segments = [];
      console.warn("[gpt-wrapper] pending Submit cancelled");
    },

    requestExplicitSend() {
      // A completed Send must not block later manually typed drafts, while
      // a still-identical composer is guarded against duplicate BLE/key taps.
      clearSubmittedDraftGuardIfChanged();
      if (state.sendRequested || state.timer !== null) {
        console.info("[gpt-wrapper] duplicate Submit request ignored");
        return "already-pending";
      }
      state.sendRequested = true;
      const started = Date.now();
      const generation = ++state.generation;
      let lastText = null;
      let stableSince = 0;
      let lastSegmentVersion = state.segmentVersion;
      console.info("[gpt-wrapper] Submit requested; delivery remains unconfirmed");

      const check = () => {
        if (generation !== state.generation) return;
        const current = composerText();
        const send = sendButton();
        const enabled = !!send && visible(send) && send.isConnected &&
          !send.disabled && send.getAttribute("aria-disabled") !== "true";
        const busy = transcriptionBusy();
        for (const segment of state.segments) {
          if (busy === true) segment.busySeen = true;
          // Prefer a real busy-to-idle transition if the page exposes one.
          // ChatGPT currently provides no verified public completion selector;
          // when absent, require changed text plus a conservative settle window
          // after the Rust controller verified the Finish button disappeared.
          const observedCompletion = segment.busySeen && busy === false;
          const fallbackCompletion = busy !== true &&
            Date.now() - segment.finishedAt >= 2000;
          if (current !== segment.before && (observedCompletion || fallbackCompletion)) {
            segment.settled = true;
          }
        }
        const settled = !state.activeSegment && busy !== true &&
          state.segments.every((segment) => segment.settled);
        if (current !== lastText || !enabled || !settled ||
            lastSegmentVersion !== state.segmentVersion) {
          lastText = current;
          stableSince = Date.now();
          lastSegmentVersion = state.segmentVersion;
        }
        if (current && settled && enabled &&
            current !== state.lastClickedText && Date.now() - stableSince >= 1000) {
          cancelTimer();
          state.pendingTranscription = false;
          state.segments = [];
          state.lastClickedText = current;
          state.sendClicked = true;
          send.click();
          console.info("[gpt-wrapper] Send button clicked; delivery unconfirmed");
          return;
        }
        if (Date.now() - started >= 15000) {
          cancelTimer();
          state.sendRequested = false;
          console.warn("[gpt-wrapper] Submit timed out: transcript/draft/Send unavailable; draft remains unsent");
        }
      };
      state.timer = setInterval(check, 100);
      check();
      return "requested";
    },

    waitAndSend() {
      cancelTimer();
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
          state.timer = null;
          send.click();
          return;
        }

        if (Date.now() - started > 15000) {
          clearInterval(state.timer);
          state.timer = null;
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
