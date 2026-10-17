const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");

const script = fs.readFileSync(path.join(__dirname, "../src/chatgpt_adapter.js"), "utf8");

function harness(initialText = "", sendLabel = "Send") {
  let now = 0;
  let nextTimer = 0;
  const timers = new Map();
  let busy = false;
  let indicatorPresent = true;
  const documentListeners = {};
  const indicator = { getAttribute(name) { return name === "aria-busy" ? String(busy) : null; } };
  const composer = {
    value: initialText,
    getAttribute(name) { return name === "aria-busy" && busy ? "true" : null; },
    closest() { return null; },
  };
  const send = {
    disabled: false,
    isConnected: true,
    clicks: 0,
    getAttribute(name) { return name === "aria-label" ? sendLabel : null; },
    getBoundingClientRect() { return { width: 20, height: 20 }; },
    click() { this.clicks++; },
  };
  const document = {
    readyState: "loading",
    querySelector(selector) {
      if (selector === "textarea#mobile-composer-prompt") return composer;
      if (selector === '[data-testid="transcription-status"]') return indicatorPresent ? indicator : null;
      return null;
    },
    querySelectorAll(selector) { return selector === "button" || selector === "button[aria-label]" ? [send] : []; },
    addEventListener(type, callback) { documentListeners[type] = callback; },
  };
  const window = { addEventListener() {} };
  const context = {
    window, document,
    console: { info() {}, warn() {}, debug() {}, error() {} },
    Date: class extends Date { static now() { return now; } },
    setInterval(callback, delay) { const id = ++nextTimer; timers.set(id, { callback, delay, due: now + delay }); return id; },
    clearInterval(id) { timers.delete(id); },
  };
  vm.runInNewContext(script, context);
  return {
    api: window.__GPT_WRAPPER__, composer, send,
    setBusy(value) { busy = value; },
    emitInput() { documentListeners.input?.(); },
    removeIndicator() { indicatorPresent = false; },
    advance(ms) {
      const end = now + ms;
      while (true) {
        const due = [...timers.entries()].filter(([, timer]) => timer.due <= end)
          .sort((a, b) => a[1].due - b[1].due)[0];
        if (!due) break;
        now = due[1].due;
        if (timers.has(due[0])) {
          due[1].due += due[1].delay;
          due[1].callback();
        }
      }
      now = end;
    },
  };
}

{
  const h = harness("Message prefix");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  assert.equal(h.api.requestExplicitSend(), "requested");
  assert.equal(h.api.requestExplicitSend(), "already-pending");
  h.advance(3000);
  assert.equal(h.send.clicks, 0, "existing prefix must not count as transcription");
  h.composer.value = "Message prefix spoken words";
  h.setBusy(false);
  h.advance(1200);
  assert.equal(h.send.clicks, 1);
  assert.equal(h.api.requestExplicitSend(), "already-pending");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  h.composer.value = "Message prefix spoken words next draft";
  h.setBusy(false);
  assert.equal(h.api.requestExplicitSend(), "requested");
  h.advance(1200);
  assert.equal(h.send.clicks, 2, "a later draft may be sent once");
}

{
  const h = harness("first segment");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  h.composer.value = "first segment second segment";
  h.setBusy(false);
  h.api.requestExplicitSend();
  h.advance(1200);
  assert.equal(h.send.clicks, 1, "multiple segments should send together");
}

{
  const h = harness("");
  h.api.requestExplicitSend();
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "empty draft must time out without a click");
  h.composer.value = "manual draft";
  h.api.requestExplicitSend();
  h.advance(1200);
  assert.equal(h.send.clicks, 1);
}

{
  const h = harness("previous segment");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  h.api.requestExplicitSend();
  h.composer.value = "previous segment new words";
  h.api.cancelExplicitSend();
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "disconnect cancels a pending send");
}

{
  const h = harness("typed draft");
  h.send.disabled = true;
  h.api.requestExplicitSend();
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "disabled Send must never be clicked");
}

{
  const h = harness("prefix");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  h.api.requestExplicitSend();
  h.composer.value = "prefix prior segment";
  h.advance(2500);
  assert.equal(h.send.clicks, 0, "earlier text cannot complete a busy latest segment");
  h.setBusy(false);
  h.advance(1200);
  assert.equal(h.send.clicks, 1);
}

{
  const h = harness("first");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  h.api.requestExplicitSend();
  h.api.prepareStart();
  h.composer.value = "first second";
  h.setBusy(false);
  h.advance(2000);
  assert.equal(h.send.clicks, 0, "new active segment keeps queued Submit waiting");
  h.setBusy(true);
  h.api.finishPttSegment();
  h.advance(200);
  h.setBusy(false);
  h.advance(1200);
  assert.equal(h.send.clicks, 1);
  assert.equal(h.api.requestExplicitSend(), "already-pending");
}

{
  const h = harness("first");
  h.api.prepareStart();
  h.removeIndicator();
  h.api.finishPttSegment();
  h.api.requestExplicitSend();
  h.composer.value = "first second";
  h.advance(1900);
  assert.equal(h.send.clicks, 0, "without an explicit completion signal, Submit must wait beyond the Finish settle window");
  h.advance(1100);
  assert.equal(h.send.clicks, 1, "missing unverified test ID must not disable device Submit forever");
}

{
  const h = harness("unchanged");
  h.api.prepareStart();
  h.removeIndicator();
  h.api.finishPttSegment();
  h.api.requestExplicitSend();
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "no new transcript must not send old draft even with fallback");
}

{
  const h = harness("manual draft", "傳送");
  assert.equal(h.api.requestExplicitSend(), "requested");
  h.advance(1200);
  assert.equal(h.send.clicks, 1, "Traditional Chinese Send label must be supported");
  assert.equal(h.api.requestExplicitSend(), "already-pending", "same draft cannot submit twice");
  h.composer.value = "another manually typed draft";
  assert.equal(h.api.requestExplicitSend(), "requested");
  h.advance(1200);
  assert.equal(h.send.clicks, 2, "newly typed draft after a successful Send must be allowed");
  h.composer.value = "";
  h.emitInput();
  h.composer.value = "another manually typed draft";
  h.emitInput();
  assert.equal(h.api.requestExplicitSend(), "requested");
  h.advance(1200);
  assert.equal(h.send.clicks, 3, "same text in a new draft after composer cleared is not a duplicate");
}

{
  const h = harness("draft", "Stop");
  h.api.requestExplicitSend();
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "must only click a recognized Send control");
}

{
  const h = harness("");
  h.api.prepareStart();
  h.removeIndicator();
  h.api.requestExplicitSend();
  h.composer.value = "speech appeared before release";
  h.advance(4000);
  assert.equal(h.send.clicks, 0, "pending dictation must never be submitted while held");
  h.api.finishPttSegment();
  h.advance(3500);
  assert.equal(h.send.clicks, 1, "a held Submit should proceed after Finish and fallback settling");
}

{
  const h = harness("first");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishPttSegment();
  h.api.requestExplicitSend();
  h.composer.value = "first second";
  h.api.cancelExplicitSend(); // pagehide/navigation calls the same cancellation path
  h.setBusy(false);
  h.advance(16000);
  assert.equal(h.send.clicks, 0);
}

{
  const h = harness("original draft");
  h.api.prepareStart();
  h.removeIndicator();
  h.api.finishPttSegment(); // keyboard and device share this callback
  h.composer.value = "original draft spoken text";
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "PTT completion alone never submits when auto-submit is off");
  assert.equal(h.composer.value, "original draft spoken text", "draft stays editable");
  assert.equal(h.api.requestExplicitSend(), "requested", "explicit Submit remains available");
  h.advance(3500);
  assert.equal(h.send.clicks, 1);
}

{
  const h = harness("prefix");
  h.api.prepareStart();
  h.removeIndicator();
  h.api.finishPttSegment();
  h.api.requestExplicitSend(); // both PTT sources call this when auto-submit is on
  h.composer.value = "prefix dictation";
  h.advance(3500);
  assert.equal(h.send.clicks, 1, "auto-submit uses the same guarded Send path");
}

{
  const h = harness("prefix");
  h.api.prepareStart();
  h.api.requestExplicitSend(); // queued explicit Submit, already dispatched in another code path
  h.api.finishPttSegment(true); // native Finish control submitted to ChatGPT directly
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "native send-on-finish must cancel pending duplicate Send");
  h.api.requestExplicitSend();
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "same draft must not send again after native Submit");
}

console.log("chatgpt_adapter explicit-send and unified PTT tests passed");
