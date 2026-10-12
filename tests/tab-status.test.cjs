const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const script = fs.readFileSync(path.join(__dirname, "../src-tauri/src/chatgpt_adapter.js"), "utf8");

function harness(channel = true) {
  const labels = [];
  const sent = [];
  const timers = [];
  let observe;
  let fail = false;
  const button = (label) => ({
    getAttribute: (name) => name === "aria-label" ? label : null,
    getBoundingClientRect: () => ({ width: 10, height: 10 }),
  });
  const document = {
    readyState: "complete",
    documentElement: {},
    addEventListener: () => {},
    querySelector: () => null,
    querySelectorAll: (selector) => selector.startsWith("button") ? labels.map(button) : [],
  };
  const webview = { postMessage(value) { if (fail) throw new Error("delivery failed"); sent.push(value); } };
  const window = {
    chrome: channel ? { webview } : undefined,
    addEventListener: () => {},
  };
  class MutationObserver {
    constructor(callback) { observe = callback; }
    observe() {}
    disconnect() {}
  }
  vm.runInNewContext(script, {
    window, document, MutationObserver,
    setTimeout: (callback) => { timers.push(callback); return timers.length; },
    clearTimeout: () => {},
    console: { debug: () => {} },
  });
  return {
    labels, sent, window,
    setFail: (value) => { fail = value; },
    change(...next) {
      labels.splice(0, labels.length, ...next);
      observe();
      timers.shift()();
    },
  };
}

const h = harness();
assert.deepEqual(JSON.parse(h.sent[0]), { kind: "gpt-wrapper-tab-status", status: "fallback" });
assert.equal(typeof h.sent[0], "string");
h.change("Start Voice");
h.change("Send", "Start Voice");
h.change("Stop", "Send", "Start Voice");
h.change("Stop", "Send", "Start Voice");
h.change("停止", "傳送", "開始語音對話");
h.change("傳送", "開始語音對話");
h.change("開始語音對話");
assert.deepEqual(h.sent.map((raw) => JSON.parse(raw).status), [
  "fallback", "voice", "send", "working", "send", "voice",
]);
h.window.__GPT_WRAPPER__.reportStatus();
assert.equal(JSON.parse(h.sent.at(-1)).status, "voice");
assert.equal(h.sent.length, 7);

const missing = harness(false);
assert.equal(missing.sent.length, 0);
missing.window.chrome = { webview: { postMessage: (raw) => missing.sent.push(raw) } };
missing.change("Start Voice");
assert.equal(JSON.parse(missing.sent[0]).status, "voice");

const retry = harness();
retry.setFail(true);
retry.change("Send");
retry.setFail(false);
retry.change("Send");
assert.deepEqual(retry.sent.map((raw) => JSON.parse(raw).status), ["fallback", "send"]);

console.log("tab status adapter tests passed");
