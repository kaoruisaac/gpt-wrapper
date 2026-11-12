const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const claude = fs.readFileSync(path.join(__dirname, "../src/claude_adapter.js"), "utf8");
const chatgpt = fs.readFileSync(path.join(__dirname, "../src/chatgpt_adapter.js"), "utf8");

function control(label) {
  return {
    label, shown: false, disabled: false, clicks: 0, isConnected: true,
    getAttribute(name) {
      if (name === "aria-label") return this.label;
      if (name === "aria-disabled") return this.disabled ? "true" : null;
      return null;
    },
    getBoundingClientRect() { return this.shown ? { width: 20, height: 20 } : { width: 0, height: 0 }; },
    click() { this.clicks++; },
  };
}

function harness({ hostname = "claude.ai", text = "", top = true, script = claude } = {}) {
  let now = 0, nextTimer = 0;
  const timers = new Map();
  const messages = [];
  const controls = {
    "口述輸入": control("口述輸入"),
    "完成聽寫": control("完成聽寫"),
    "傳送訊息": control("傳送訊息"),
    "停止回應": control("停止回應"),
  };
  controls["口述輸入"].shown = true;
  const composer = { innerText: text, isConnected: true, isContentEditable: true };
  let observerCallback = null;
  const document = {
    readyState: "complete",
    documentElement: {},
    querySelector(selector) {
      return selector === 'div.ProseMirror[contenteditable="true"]' ? composer : null;
    },
    querySelectorAll(selector) {
      const match = selector.match(/^\[aria-label="(.+)"\]$/);
      return match && controls[match[1]] ? [controls[match[1]]] : [];
    },
    addEventListener() {},
  };
  const window = {
    chrome: { webview: { postMessage: (raw) => messages.push(JSON.parse(raw)) } },
    addEventListener() {},
  };
  window.top = top ? window : {};
  const schedule = (repeat) => (callback, delay = 0) => {
    const id = ++nextTimer;
    timers.set(id, { callback, delay, due: now + delay, repeat });
    return id;
  };
  vm.runInNewContext(script, {
    window, document, location: { protocol: "https:", hostname },
    console: { info() {}, warn() {}, debug() {}, error() {} },
    Date: class extends Date { static now() { return now; } },
    setInterval: schedule(true), setTimeout: schedule(false),
    clearInterval: (id) => timers.delete(id), clearTimeout: (id) => timers.delete(id),
    MutationObserver: class {
      constructor(callback) { observerCallback = callback; }
      observe() {}
      disconnect() {}
    },
  });
  return {
    api: window.__GPT_WRAPPER__, controls, composer, messages,
    statuses: () => messages.filter((m) => m.kind === "gpt-wrapper-tab-status").map((m) => m.status),
    mutate() { observerCallback?.(); },
    advance(ms) {
      const end = now + ms;
      while (true) {
        const due = [...timers.entries()].filter(([, t]) => t.due <= end)
          .sort((a, b) => a[1].due - b[1].due)[0];
        if (!due) break;
        const [id, timer] = due;
        now = timer.due;
        if (timer.repeat) timer.due += timer.delay; else timers.delete(id);
        timer.callback();
      }
      now = end;
    },
  };
}

test("adapters only install in their own trusted top-level origin", () => {
  assert.equal(harness().api.provider, "claude");
  for (const hostname of ["chatgpt.com", "claude.ai.evil.com", "accounts.google.com"]) {
    assert.equal(harness({ hostname }).api, undefined, hostname);
  }
  assert.equal(harness({ top: false }).api, undefined, "iframes never get DOM control");
  assert.equal(harness({ script: chatgpt, hostname: "claude.ai" }).api, undefined,
    "the ChatGPT adapter must not run on Claude");
  assert.equal(harness({ script: chatgpt, hostname: "chatgpt.com" }).api.provider, "chatgpt");
});

test("auto-submit waits for dictation end, changed text and a stable draft", () => {
  const h = harness({ text: "prefix" });
  h.api.prepareStart();
  h.controls["完成聽寫"].shown = true;
  h.api.finishPttSegment();
  h.controls["傳送訊息"].shown = true;
  assert.equal(h.api.requestExplicitSend(), "requested");
  assert.equal(h.api.requestExplicitSend(), "already-pending");
  h.advance(5000);
  assert.equal(h.controls["傳送訊息"].clicks, 0, "never send while dictation is still active");
  h.controls["完成聽寫"].shown = false;
  h.advance(3000);
  assert.equal(h.controls["傳送訊息"].clicks, 0, "an unchanged draft is not a transcript");
  h.composer.innerText = "prefix spoken";
  h.advance(600);
  h.composer.innerText = "prefix spoken words";
  h.advance(900);
  assert.equal(h.controls["傳送訊息"].clicks, 0, "text still settling");
  h.advance(300);
  assert.equal(h.controls["傳送訊息"].clicks, 1);
  assert.equal(h.api.requestExplicitSend(), "already-pending", "duplicate Submit taps are ignored");
  h.advance(3000);
  assert.equal(h.controls["傳送訊息"].clicks, 1, "the same draft is never sent twice");
});

test("timeout keeps the draft unsent and disabled Send blocks submission", () => {
  const h = harness({ text: "" });
  h.api.prepareStart();
  h.api.finishPttSegment();
  h.composer.innerText = "transcript";
  h.controls["傳送訊息"].shown = true;
  h.controls["傳送訊息"].disabled = true;
  h.api.requestExplicitSend();
  h.advance(16000);
  assert.equal(h.controls["傳送訊息"].clicks, 0);
  assert.equal(h.composer.innerText, "transcript");
  h.controls["傳送訊息"].disabled = false;
  h.advance(3000);
  assert.equal(h.controls["傳送訊息"].clicks, 0, "a timed-out request does not linger");
});

test("a reply in progress is not a finished transcript", () => {
  const h = harness({ text: "" });
  h.api.prepareStart();
  h.api.finishPttSegment();
  h.composer.innerText = "words";
  h.controls["傳送訊息"].shown = true;
  h.controls["停止回應"].shown = true;
  h.api.requestExplicitSend();
  h.advance(5000);
  assert.equal(h.controls["傳送訊息"].clicks, 0);
  h.controls["停止回應"].shown = false;
  h.advance(1200);
  assert.equal(h.controls["傳送訊息"].clicks, 1);
});

test("cancel drops a queued Submit", () => {
  const h = harness({ text: "" });
  h.api.prepareStart();
  h.api.finishPttSegment();
  h.controls["傳送訊息"].shown = true;
  h.api.requestExplicitSend();
  h.api.cancelExplicitSend();
  h.composer.innerText = "words";
  h.advance(5000);
  assert.equal(h.controls["傳送訊息"].clicks, 0);
});

test("status follows Claude controls and debounces the Working exit", () => {
  const h = harness();
  assert.deepEqual(h.statuses(), ["voice"]);
  h.controls["停止回應"].shown = true;
  h.mutate();
  h.advance(150);
  assert.deepEqual(h.statuses(), ["voice", "working"]);
  // A short re-render that briefly removes Stop is not a completion.
  h.controls["停止回應"].shown = false;
  h.mutate();
  h.advance(150);
  h.controls["停止回應"].shown = true;
  h.mutate();
  h.advance(1000);
  assert.deepEqual(h.statuses(), ["voice", "working"]);
  h.controls["停止回應"].shown = false;
  h.controls["傳送訊息"].shown = true;
  h.mutate();
  h.advance(400);
  assert.deepEqual(h.statuses(), ["voice", "working"]);
  h.advance(600);
  assert.deepEqual(h.statuses(), ["voice", "working", "send"]);
});
