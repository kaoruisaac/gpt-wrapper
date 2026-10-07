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
    const labels = ["傳送訊息", "Send message", "Send prompt"];
    return buttons().find((b) => visible(b) && labels.includes(aria(b))) ||
      q('[data-testid="send-button"]') ||
      q("#composer-submit-button");
  };

  window.__GPT_WRAPPER__ = {
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
})();