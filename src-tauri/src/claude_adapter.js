// Claude (claude.ai) DOM adapter. Exposes the same window.__GPT_WRAPPER__
// contract as chatgpt_adapter.js so the Rust PTT controller stays shared.
// Only runs in the trusted top-level claude.ai document.
(() => {
  if (window !== window.top || location.protocol !== "https:" ||
      location.hostname !== "claude.ai" || window.__GPT_WRAPPER__) return;

  // Initial selectors fixed by the dual-provider contract.
  const LABELS = Object.freeze({
    start: "口述輸入",
    finish: "完成聽寫",
    send: "傳送訊息",
    stop: "停止回應",
  });
  // Claude exposes no verified transcription-complete signal. A segment is
  // considered settled only after the Finish control is gone, the draft text
  // changed, and this minimum delay has passed; then the draft must stay
  // unchanged for STABLE_MS with an enabled Send button.
  const SEGMENT_MIN_MS = 1500;
  const STABLE_MS = 1000;
  const SUBMIT_TIMEOUT_MS = 15000;
  // Reply completion: a missing Stop button must persist this long before the
  // tab leaves "working", so short re-renders do not look like completion.
  const WORKING_EXIT_MS = 700;

  const state = {
    before: "", timer: null, sendRequested: false, lastClickedText: null,
    generation: 0, activeSegment: false, segments: [], segmentVersion: 0,
    sendClicked: false,
  };
  const cancelTimer = () => {
    if (state.timer !== null) clearInterval(state.timer);
    state.timer = null;
    state.generation++;
  };
  const visible = (el) => {
    if (!el || el.isConnected === false) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const enabled = (el) => !!el && !el.disabled && el.getAttribute("aria-disabled") !== "true";
  const control = (label) =>
    [...document.querySelectorAll(`[aria-label="${label}"]`)].find(visible) || null;

  const composer = () =>
    document.querySelector('[data-testid="chat-input"][contenteditable="true"]') ||
    document.querySelector('div.ProseMirror[contenteditable="true"]') ||
    document.querySelector('fieldset [contenteditable="true"]') ||
    document.querySelector('[contenteditable="true"][role="textbox"]') ||
    document.querySelector("fieldset textarea");

  const composerText = () => {
    const el = composer();
    return (el?.value ?? el?.innerText ?? el?.textContent ?? "").trim();
  };

  const dictationActive = () => !!control(LABELS.finish);

  const clearSubmittedDraftGuardIfChanged = () => {
    if (!state.sendClicked || state.activeSegment || composerText() === state.lastClickedText) return;
    state.sendClicked = false;
    state.sendRequested = false;
    state.lastClickedText = null;
  };
  document.addEventListener("input", clearSubmittedDraftGuardIfChanged, true);

  const post = (status) => {
    const webview = window.chrome?.webview;
    if (!webview?.postMessage) return false;
    try {
      webview.postMessage(JSON.stringify({ kind: "gpt-wrapper-tab-status", status }));
      return true;
    } catch (error) {
      console.debug("[gpt-wrapper] tab status delivery failed", error);
      return false;
    }
  };

  const rawStatus = () => {
    if (control(LABELS.stop)) return "working";
    const send = control(LABELS.send);
    if (send && enabled(send)) return "send";
    if (control(LABELS.start)) return "voice";
    return "fallback";
  };

  const startStatusObserver = () => {
    let lastStatus = null;
    let pending = null;
    let exitTimer = null;
    const update = () => {
      const status = rawStatus();
      if (status === lastStatus) {
        if (exitTimer !== null) { clearTimeout(exitTimer); exitTimer = null; }
        return;
      }
      if (lastStatus === "working" && status !== "working") {
        // Confirm the Stop control stays gone before leaving Working.
        if (exitTimer === null) {
          exitTimer = setTimeout(() => {
            exitTimer = null;
            const confirmed = rawStatus();
            if (confirmed !== "working" && confirmed !== lastStatus && post(confirmed)) {
              lastStatus = confirmed;
            }
          }, WORKING_EXIT_MS);
        }
        return;
      }
      if (exitTimer !== null) { clearTimeout(exitTimer); exitTimer = null; }
      if (post(status)) lastStatus = status;
    };
    window.__GPT_WRAPPER__.reportStatus = () => {
      lastStatus = null;
      if (exitTimer !== null) { clearTimeout(exitTimer); exitTimer = null; }
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
      if (exitTimer !== null) clearTimeout(exitTimer);
    }, { once: true });
  };

  const focusComposerAtEnd = (el) => {
    el.focus({ preventScroll: true });
    if (el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement) {
      const end = el.value.length;
      el.setSelectionRange(end, end);
      return document.activeElement === el && el.selectionStart === end && el.selectionEnd === end;
    }
    if (el.isContentEditable) {
      const selection = window.getSelection();
      if (!selection) return false;
      const range = document.createRange();
      range.selectNodeContents(el);
      range.collapse(false);
      selection.removeAllRanges();
      selection.addRange(range);
      return document.activeElement === el && selection.rangeCount === 1 &&
        selection.getRangeAt(0).collapsed;
    }
    return false;
  };
  const sameText = (a, b) => a.replace(/\s+/g, " ").trim() === b.replace(/\s+/g, " ").trim();

  window.__GPT_WRAPPER__ = {
    provider: "claude",

    showRecoveryStatus(message) {
      let notice = document.getElementById("gpt-wrapper-ptt-recovery");
      if (!notice) {
        notice = document.createElement("div");
        notice.id = "gpt-wrapper-ptt-recovery";
        notice.setAttribute("role", "status");
        Object.assign(notice.style, {
          position: "fixed", right: "16px", bottom: "20px", zIndex: "2147483647",
          maxWidth: "360px", padding: "10px 14px", borderRadius: "9px",
          background: "#303030", color: "#fff", fontSize: "13px",
          boxShadow: "0 4px 18px #0005", pointerEvents: "none",
        });
        document.body?.append(notice);
      }
      notice.textContent = message;
      const version = Number(notice.dataset.version || 0) + 1;
      notice.dataset.version = String(version);
      setTimeout(() => {
        if (notice.isConnected && Number(notice.dataset.version) === version) notice.remove();
      }, 4500);
    },

    // Claude's composer is a ProseMirror editor; insert through the editing
    // pipeline so the editor state (and Send availability) follows the text.
    applyMessagePrefix(prefix) {
      if (prefix === "") return "success";
      const el = composer();
      if (!el || !el.isConnected) return "not-ready";
      try {
        if (el instanceof HTMLTextAreaElement) {
          if (el.disabled || el.readOnly) return "not-ready";
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value").set.call(el, prefix);
          el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: prefix }));
          if (el.value !== prefix) return "failure";
          return focusComposerAtEnd(el) ? "success" : "failure";
        }
        if (!el.isContentEditable) return "not-ready";
        el.focus({ preventScroll: true });
        const selection = window.getSelection();
        if (!selection) return "failure";
        const range = document.createRange();
        range.selectNodeContents(el);
        selection.removeAllRanges();
        selection.addRange(range);
        const inserted = document.execCommand("insertText", false, prefix);
        const actual = el.innerText ?? el.textContent ?? "";
        if (!inserted || !sameText(actual, prefix)) return "failure";
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
      if (!state.sendRequested) cancelTimer();
      state.before = composerText();
      state.activeSegment = true;
      state.segmentVersion++;
      if (!state.sendRequested) state.lastClickedText = null;
    },

    // Claude's Finish control only ends dictation; Rust never reports a
    // native send for this provider. Keep the parameter for contract parity.
    finishPttSegment(nativeSent = false) {
      state.activeSegment = false;
      if (nativeSent) {
        cancelTimer();
        state.sendRequested = false;
        state.segments = [];
        state.segmentVersion++;
        state.lastClickedText = composerText();
        state.sendClicked = true;
        return;
      }
      state.segments.push({ before: state.before, finishedAt: Date.now(), settled: false });
      state.segmentVersion++;
      console.info("[gpt-wrapper] Claude PTT Finish processed; awaiting transcript readiness");
    },

    cancelExplicitSend() {
      cancelTimer();
      state.sendRequested = false;
      state.sendClicked = false;
      state.activeSegment = false;
      state.segments = [];
      console.warn("[gpt-wrapper] pending Submit cancelled");
    },

    requestExplicitSend() {
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

      const check = () => {
        if (generation !== state.generation) return;
        const current = composerText();
        const send = control(LABELS.send);
        const sendReady = !!send && enabled(send);
        const dictating = dictationActive();
        const replying = !!control(LABELS.stop);
        for (const segment of state.segments) {
          if (!dictating && current !== segment.before &&
              Date.now() - segment.finishedAt >= SEGMENT_MIN_MS) {
            segment.settled = true;
          }
        }
        const settled = !state.activeSegment && !dictating &&
          state.segments.every((segment) => segment.settled);
        if (current !== lastText || !sendReady || !settled || replying ||
            lastSegmentVersion !== state.segmentVersion) {
          lastText = current;
          stableSince = Date.now();
          lastSegmentVersion = state.segmentVersion;
        }
        if (current && settled && sendReady && !replying &&
            current !== state.lastClickedText && Date.now() - stableSince >= STABLE_MS) {
          cancelTimer();
          state.segments = [];
          state.lastClickedText = current;
          state.sendClicked = true;
          send.click();
          console.info("[gpt-wrapper] Claude Send clicked; delivery unconfirmed");
          return;
        }
        if (Date.now() - started >= SUBMIT_TIMEOUT_MS) {
          cancelTimer();
          state.sendRequested = false;
          console.warn("[gpt-wrapper] Submit timed out: transcript/draft/Send unavailable; draft remains unsent");
        }
      };
      state.timer = setInterval(check, 100);
      check();
      return "requested";
    },
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", startStatusObserver, { once: true });
  } else {
    startStatusObserver();
  }
})();
