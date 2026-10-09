const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../src/microphone_inventory.js"), "utf8");

function harness(hostname, config = {}) {
  const messages = [];
  let enumerated = 0, captured = 0;
  const mediaDevices = {
    getUserMedia: () => { captured++; throw Error("inventory must never access the microphone"); },
    enumerateDevices: async () => {
      enumerated++;
      if (config.reject) throw { name: "NotAllowedError" };
      return [
        { kind: "audioinput", deviceId: "default", groupId: "", label: "Default" },
        { kind: "audioinput", deviceId: "usb-1", groupId: "g1", label: config.private ? "" : "USB Mic" },
        { kind: "audioinput", deviceId: "usb-2", groupId: "g2", label: config.private ? "" : "USB Mic" },
        { kind: "audiooutput", deviceId: "speakers", groupId: "x", label: "Speakers" },
      ];
    },
  };
  const window = { chrome: { webview: { postMessage: value => messages.push(JSON.parse(value)) } } };
  window.top = window;
  const context = { window, location: { protocol: "https:", hostname }, navigator: { mediaDevices } };
  vm.runInNewContext(source, context);
  return {
    window, messages,
    get enumerated() { return enumerated; },
    get captured() { return captured; },
  };
}

(async () => {
  const untrusted = harness("example.com");
  assert.equal(untrusted.window.__GPT_WRAPPER_MIC_INVENTORY__, undefined);
  assert.equal(untrusted.enumerated, 0);

  const h = harness("chatgpt.com");
  assert.equal(h.enumerated, 0, "no implicit permission prompt or enumeration");
  await h.window.__GPT_WRAPPER_MIC_INVENTORY__.query("request-1");
  const result = h.messages[0];
  assert.equal(result.kind, "gpt-wrapper-microphone-inventory");
  assert.equal(result.requestId, "request-1");
  assert.equal(result.devices.length, 2, "exclude speakers and virtual default pseudo-device");
  assert.equal(result.devices[0].groupId, "g1");
  assert.equal(h.captured, 0);
  vm.runInNewContext(source, { window: h.window, location: { protocol: "https:", hostname: "chatgpt.com" }, navigator: { mediaDevices: {} } });
  assert.equal(typeof h.window.__GPT_WRAPPER_MIC_INVENTORY__.query, "function", "idempotent injection");

  const privateDevices = harness("chatgpt.com", { private: true });
  await privateDevices.window.__GPT_WRAPPER_MIC_INVENTORY__.query("request-2");
  assert.equal(privateDevices.messages[0].devices[0].label, "", "do not invent device names");

  const denied = harness("chatgpt.com", { reject: true });
  await denied.window.__GPT_WRAPPER_MIC_INVENTORY__.query("request-3");
  assert.equal(denied.messages[0].error, "NotAllowedError");
  assert.equal(denied.captured, 0);
  console.log("Microphone inventory tests passed (trusted origin, device filtering, privacy, idempotence, no capture)");
})().catch(error => { console.error(error); process.exitCode = 1; });
