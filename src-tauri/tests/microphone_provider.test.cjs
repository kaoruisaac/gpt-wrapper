const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const routeSource = fs.readFileSync(path.join(__dirname, "../src/microphone_route.js"), "utf8");
const inventorySource = fs.readFileSync(path.join(__dirname, "../src/microphone_inventory.js"), "utf8");
const MARKER = "/*GPT_WRAPPER_PROVIDER*/null";
const pin = (source, provider) => source.replace(MARKER, JSON.stringify(provider));

function route({ hostname = "claude.ai", pinned = "claude", inputs = [] } = {}) {
  const messages = [], listeners = {}, calls = [];
  const document = { body: null, addEventListener(type, fn) { listeners[type] = fn; } };
  const mediaDevices = {
    getUserMedia(constraints) {
      calls.push(constraints);
      const id = constraints.audio?.deviceId?.exact || "native";
      const track = { getSettings: () => ({ deviceId: id }), addEventListener() {}, stop() {} };
      return Promise.resolve({ getAudioTracks: () => [track], getTracks: () => [track] });
    },
    enumerateDevices: async () => inputs,
  };
  const window = {
    chrome: { webview: { postMessage: (raw) => messages.push(JSON.parse(raw)) } },
    addEventListener() {},
  };
  window.top = window;
  const source = pinned ? pin(routeSource, pinned) : routeSource;
  vm.runInNewContext(source, {
    window, document, navigator: { mediaDevices },
    location: { protocol: "https:", hostname },
    console, DOMException, Promise, Date,
  });
  const click = (label) => {
    let prevented = false;
    listeners.click?.({
      target: { closest: () => ({ getAttribute: () => label }) },
      preventDefault() { prevented = true; },
      stopImmediatePropagation() {},
    });
    return prevented;
  };
  const routeEvents = () => messages.filter((m) => m.kind === "gpt-wrapper-microphone-route");
  return { api: window.__GPT_WRAPPER_MIC_ROUTE__, mediaDevices, calls, click, routeEvents };
}
const flush = () => new Promise((resolve) => setImmediate(resolve));

test("routing is pinned to the tab provider's own origin", () => {
  assert.ok(route().api, "Claude tab on claude.ai");
  assert.equal(route({ hostname: "chatgpt.com" }).api, undefined,
    "a Claude tab that navigates to ChatGPT gets no routing");
  assert.equal(route({ hostname: "claude.ai", pinned: "chatgpt" }).api, undefined);
  assert.equal(route({ hostname: "claude.ai.example.com", pinned: null }).api, undefined);
  assert.ok(route({ hostname: "chatgpt.com", pinned: "chatgpt" }).api);
});

test("Claude Dictation uses the exact confirmed Claude device ID", async () => {
  const h = route();
  h.api.configure({ mode: "device", deviceId: "claude-B", label: "USB B", groupId: "" }, 2);
  assert.deepEqual(h.routeEvents().map((m) => m.event), ["ready"]);
  assert.equal(h.click("口述輸入"), false);
  const stream = await h.mediaDevices.getUserMedia({ audio: true });
  assert.equal(h.calls[0].audio.deviceId.exact, "claude-B");
  assert.ok(stream);
  // ChatGPT labels are not Claude dictation gestures.
  const other = route();
  other.api.configure({ mode: "device", deviceId: "claude-B", label: "USB B", groupId: "" }, 2);
  other.click("聽寫");
  await other.mediaDevices.getUserMedia({ audio: { echoCancellation: true } });
  assert.equal(other.calls[0].audio.deviceId, undefined, "unclassified capture stays native");
});

test("a choice from another origin is matched by its unique label", async () => {
  const h = route({
    inputs: [
      { kind: "audioinput", deviceId: "default", label: "Default - USB B" },
      { kind: "audioinput", deviceId: "c-A", label: "USB A" },
      { kind: "audioinput", deviceId: "c-B", label: "USB B" },
    ],
  });
  h.api.configure({ mode: "device", deviceId: "", label: "USB B", groupId: "", resolve: true }, 3);
  assert.equal(h.click("口述輸入"), true, "blocked while the origin match is pending");
  await flush();
  assert.deepEqual(h.routeEvents().map((m) => m.event), ["ready"]);
  assert.equal(h.click("口述輸入"), false);
  await h.mediaDevices.getUserMedia({ audio: true });
  assert.equal(h.calls[0].audio.deviceId.exact, "c-B");
});

for (const [name, inputs, reason] of [
  ["ambiguous", [
    { kind: "audioinput", deviceId: "x1", label: "USB B" },
    { kind: "audioinput", deviceId: "x2", label: "USB B" },
  ], "ambiguous"],
  ["missing", [{ kind: "audioinput", deviceId: "x1", label: "USB A" }], "missing"],
  ["no permission", [{ kind: "audioinput", deviceId: "x1", label: "" }], "permission"],
]) {
  test(`unresolvable origin match (${name}) fails closed and never picks a device`, async () => {
    const h = route({ inputs });
    h.api.configure({ mode: "device", deviceId: "", label: "USB B", groupId: "", resolve: true }, 4);
    await flush();
    const events = h.routeEvents();
    assert.deepEqual(events.map((m) => [m.event, m.reason]), [["unresolved", reason]]);
    assert.equal(h.click("口述輸入"), true, "Dictation is blocked, not routed to Default");
    assert.equal(h.calls.length, 0);
  });
}

test("Windows Default keeps the native constraints on Claude", async () => {
  const h = route();
  h.api.configure({ mode: "default" }, 1);
  h.click("口述輸入");
  const original = { audio: { noiseSuppression: true }, video: false };
  await h.mediaDevices.getUserMedia(original);
  assert.equal(h.calls[0], original);
});

function inventory(hostname, pinned) {
  const messages = [];
  let captured = 0, stopped = 0;
  const mediaDevices = {
    getUserMedia: async () => {
      captured++;
      return { getTracks: () => [{ stop: () => { stopped++; } }] };
    },
    enumerateDevices: async () => [
      { kind: "audioinput", deviceId: "c-1", groupId: "g", label: "USB" },
    ],
  };
  const window = { chrome: { webview: { postMessage: (value) => messages.push(JSON.parse(value)) } } };
  window.top = window;
  vm.runInNewContext(pinned ? pin(inventorySource, pinned) : inventorySource, {
    window, location: { protocol: "https:", hostname }, navigator: { mediaDevices },
  });
  return { api: window.__GPT_WRAPPER_MIC_INVENTORY__, messages, captured: () => captured, stopped: () => stopped };
}

test("Claude inventory lists its own origin; only an explicit request opens the microphone", async () => {
  assert.equal(inventory("chatgpt.com", "claude").api, undefined);
  const h = inventory("claude.ai", "claude");
  await h.api.query("one");
  assert.equal(h.captured(), 0);
  assert.equal(h.messages[0].devices[0].deviceId, "c-1");
  await h.api.requestAccess("two");
  assert.equal(h.captured(), 1);
  assert.equal(h.stopped(), 1, "the permission stream is closed immediately");
  assert.equal(h.messages[1].requestId, "two");
});
