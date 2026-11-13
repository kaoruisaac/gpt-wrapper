// Shared host bridge contract: every page script posts the same JSON string
// either through WebView2 (window.chrome.webview) or, on macOS, through the
// WKScriptMessageHandler "gptWrapper" (window.webkit.messageHandlers). The
// host validates the native frame origin; scripts never run off-provider.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const src = (name) => fs.readFileSync(path.join(__dirname, "../src-tauri/src", name), "utf8");

function page({ hostname, bridge, top = true }) {
  const sent = [];
  const handler = { postMessage(value) { sent.push(value); } };
  const window = {
    addEventListener: () => {},
    chrome: bridge === "webview2" ? { webview: handler } : undefined,
    webkit: bridge === "webkit" ? { messageHandlers: { gptWrapper: handler } } : undefined,
  };
  window.top = top ? window : {};
  const document = {
    readyState: "complete",
    documentElement: {},
    body: null,
    addEventListener: () => {},
    querySelector: () => null,
    querySelectorAll: () => [],
  };
  class MutationObserver { observe() {} disconnect() {} }
  const context = {
    window, document, MutationObserver,
    location: { protocol: "https:", hostname },
    navigator: { mediaDevices: {
      getUserMedia: async () => ({ getTracks: () => [] }),
      enumerateDevices: async () => [
        { kind: "audioinput", deviceId: "mic-1", groupId: "g", label: "USB Mic" },
      ],
    } },
    setTimeout: () => 0, clearTimeout: () => {}, setInterval: () => 0, clearInterval: () => {},
    console: { debug() {}, info() {}, warn() {}, error() {} },
  };
  return { context, window, sent };
}

function run(script, env) {
  vm.runInNewContext(script, env.context);
  return env;
}

(async () => {
  // Both providers' status adapters deliver through the WebKit handler alone.
  for (const [file, hostname] of [["chatgpt_adapter.js", "chatgpt.com"], ["claude_adapter.js", "claude.ai"]]) {
    const webkit = run(src(file), page({ hostname, bridge: "webkit" }));
    assert.equal(webkit.sent.length, 1, `${file} posts its first status via WebKit`);
    assert.equal(typeof webkit.sent[0], "string");
    assert.deepEqual(JSON.parse(webkit.sent[0]), { kind: "gpt-wrapper-tab-status", status: "fallback" });

    // The WebView2 path is unchanged and takes precedence when present.
    const windows = run(src(file), page({ hostname, bridge: "webview2" }));
    assert.deepEqual(windows.sent, webkit.sent, `${file}: identical payload on both bridges`);

    // Untrusted page or subframe: the adapter never installs or posts.
    const evil = run(src(file), page({ hostname: "evil.example", bridge: "webkit" }));
    assert.equal(evil.sent.length, 0);
    assert.equal(evil.window.__GPT_WRAPPER__, undefined);
    const frame = run(src(file), page({ hostname, bridge: "webkit", top: false }));
    assert.equal(frame.sent.length, 0);
  }

  // Microphone inventory replies through the same bridge with the same shape.
  const pin = (script, id) => script.replace("/*GPT_WRAPPER_PROVIDER*/null", JSON.stringify(id));
  const inventory = run(pin(src("microphone_inventory.js"), "claude"), page({ hostname: "claude.ai", bridge: "webkit" }));
  await inventory.window.__GPT_WRAPPER_MIC_INVENTORY__.query("req-1");
  assert.equal(inventory.sent.length, 1);
  assert.deepEqual(JSON.parse(inventory.sent[0]), {
    kind: "gpt-wrapper-microphone-inventory", requestId: "req-1",
    devices: [{ deviceId: "mic-1", groupId: "g", label: "USB Mic" }],
  });
  // A tab pinned to ChatGPT that navigated to claude.ai never answers there.
  const crossed = run(pin(src("microphone_inventory.js"), "chatgpt"), page({ hostname: "claude.ai", bridge: "webkit" }));
  assert.equal(crossed.window.__GPT_WRAPPER_MIC_INVENTORY__, undefined);

  // No bridge at all (plain browser): nothing throws, nothing is sent.
  const none = run(src("chatgpt_adapter.js"), page({ hostname: "chatgpt.com", bridge: null }));
  assert.equal(none.sent.length, 0);

  console.log("WebKit / WebView2 shared bridge tests passed");
})().catch((error) => { console.error(error); process.exitCode = 1; });
