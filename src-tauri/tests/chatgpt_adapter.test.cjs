const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");

const script = fs.readFileSync(path.join(__dirname, "../src/chatgpt_adapter.js"), "utf8");

function harness(initialText = "") {
  let now = 0;
  let nextTimer = 0;
  const timers = new Map();
  let busy = false;
  let indicatorPresent = true;
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
    getAttribute(name) { return name === "aria-label" ? "Send" : null; },
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
    querySelectorAll(selector) { return selector === "button" ? [send] : []; },
    addEventListener() {},
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
  h.api.finishBleSegment();
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
  h.api.finishBleSegment();
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
  h.api.finishBleSegment();
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
  h.api.finishBleSegment();
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
  h.api.finishBleSegment();
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
  h.api.finishBleSegment();
  h.api.requestExplicitSend();
  h.api.prepareStart();
  h.composer.value = "first second";
  h.setBusy(false);
  h.advance(2000);
  assert.equal(h.send.clicks, 0, "new active segment keeps queued Submit waiting");
  h.setBusy(true);
  h.api.finishBleSegment();
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
  h.api.finishBleSegment();
  h.api.requestExplicitSend();
  h.composer.value = "first second";
  h.advance(16000);
  assert.equal(h.send.clicks, 0, "missing busy-to-idle signal must fail closed");
}

{
  const h = harness("first");
  h.api.prepareStart();
  h.setBusy(true);
  h.api.finishBleSegment();
  h.api.requestExplicitSend();
  h.composer.value = "first second";
  h.api.cancelExplicitSend(); // pagehide/navigation calls the same cancellation path
  h.setBusy(false);
  h.advance(16000);
  assert.equal(h.send.clicks, 0);
}

console.log("chatgpt_adapter explicit-send tests passed");
