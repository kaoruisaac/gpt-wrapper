const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");

const script = fs.readFileSync(path.join(__dirname, "../src/microphone_poc.js"), "utf8");
function harness(hostname = "chatgpt.com", childFrame = false) {
  const logs = [];
  const relayed = [];
  const listeners = new Map();
  let called = 0;
  const stream = {
    getAudioTracks() {
      return [{ getSettings: () => ({ deviceId: "private-B" }) }];
    },
  };
  const nativePromise = Promise.resolve(stream);
  const original = (constraints) => {
    called++;
    assert.equal(constraints.audio.echoCancellation, true);
    return nativePromise;
  };
  const mediaDevices = {
    getUserMedia: original,
    enumerateDevices: () => Promise.resolve([
      { kind: "audioinput", deviceId: "private-A", label: "Microphone A" },
      { kind: "audioinput", deviceId: "private-B", label: "Microphone B" },
    ]),
  };
  const window = { chrome: { webview: { postMessage: (message) => logs.push(JSON.parse(message)) } }, addEventListener() {} };
  window.top = childFrame ? { postMessage: (message, origin) => relayed.push({ message, origin }) } : window;
  const context = {
    window, location: { hostname, protocol: "https:", origin: `https://${hostname}` },
    navigator: { mediaDevices }, document: { addEventListener: (kind, cb) => listeners.set(kind, cb) },
    Date, Promise, Reflect,
  };
  vm.runInNewContext(script, context);
  return {
    logs, relayed, mediaDevices, original, nativePromise,
    get called() { return called; },
    click(label) {
      listeners.get("click")?.({ target: {
        closest: () => ({ getAttribute: () => label }),
      } });
    },
  };
}
(async () => {
  const invalid = harness("example.com");
  assert.equal(invalid.logs.length, 0, "unknown site must never get a probe");
  assert.equal(invalid.mediaDevices.getUserMedia, invalid.original);

  const h = harness();
  assert.equal(h.called, 0, "POC must not open the microphone or ask permission");
  assert.ok(h.logs.find(x => x.phase === "ready"));
  const constraints = { audio: { echoCancellation: true }, video: false };
  h.click("Start Voice");
  const result = h.mediaDevices.getUserMedia(constraints);
  assert.equal(result, h.nativePromise, "original promise identity must remain unchanged");
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(h.called, 1);
  assert.equal(constraints.audio.deviceId, undefined, "native constraints must not be mutated");
  const requested = h.logs.find(x => x.phase === "requested");
  assert.equal(requested.gesture, "voice");
  assert.equal(requested.hasExplicitDeviceId, false);
  const fulfilled = h.logs.find(x => x.phase === "fulfilled");
  assert.equal(fulfilled.matchedDeviceLabel, "Microphone B");
  assert.ok(!JSON.stringify(h.logs).includes("private-B"), "raw browser device IDs must not be sent");
  h.click("聽寫");
  h.mediaDevices.getUserMedia(constraints);
  assert.equal(h.logs.filter(x => x.phase === "requested").at(-1).gesture, "dictation");
  const child = harness("chatgpt.com", true);
  child.click("Dictate");
  child.mediaDevices.getUserMedia(constraints);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(child.logs.length, 0, "child frame must not send directly to the host");
  assert.ok(child.relayed.some(x => x.origin === "https://chatgpt.com" &&
    x.message.__gptWrapperMicProbe?.phase === "fulfilled"),
    "same-origin child frame should relay capture metadata to top frame");
  console.log("Microphone POC tests passed (no capture on init, original promise/constraints, origin gate, click classification, redaction)");
})().catch(error => { console.error(error); process.exitCode = 1; });
