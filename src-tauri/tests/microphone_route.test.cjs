const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "../src/microphone_route.js"), "utf8");

function harness(hostname = "chatgpt.com") {
  const messages = [], listeners = {}, calls = [], tracks = [];
  let inputs = [];
  let enumerations = 0, trackId = null, reject = null, delayed = null, now = 10000;
  const document = {
    body: null,
    addEventListener(type, fn) { listeners[type] = fn; },
  };
  const track = id => {
    const handlers = {};
    return {
      id, stopped: false, readyState: "live",
      getSettings: () => ({ deviceId: id }),
      addEventListener: (type, fn) => { handlers[type] = fn; },
      stop() { this.stopped = true; this.readyState = "ended"; handlers.ended?.(); },
      end() { this.readyState = "ended"; handlers.ended?.(); },
    };
  };
  const mediaDevices = {
    getUserMedia(constraints) {
      calls.push(constraints);
      if (reject) return Promise.reject(Object.assign(new Error(reject), { name: reject }));
      const id = trackId || constraints.audio?.deviceId?.exact || "A";
      const t = track(id); tracks.push(t);
      const stream = { getAudioTracks: () => [t], getTracks: () => [t] };
      if (delayed) return new Promise((resolve, rejectPromise) => {
        const finish = () => resolve(stream);
        finish.reject = name => rejectPromise(Object.assign(new Error(name), { name }));
        delayed.push(finish);
      });
      return Promise.resolve(stream);
    },
    enumerateDevices: async () => { enumerations++; return inputs; },
    addEventListener(type, fn) { listeners[type] = fn; },
  };
  const original = mediaDevices.getUserMedia;
  const window = {
    chrome: { webview: { postMessage: raw => messages.push(JSON.parse(raw)) } },
    addEventListener() {},
  };
  window.top = window;
  vm.runInNewContext(source, {
    window, document, navigator: { mediaDevices },
    location: { protocol: "https:", hostname },
    console, DOMException, Promise, Date: { now: () => now },
  });
  const click = label => {
    let prevented = false;
    listeners.click?.({
      target: { closest: () => ({ getAttribute: () => label }) },
      preventDefault() { prevented = true; },
      stopImmediatePropagation() {},
    });
    return prevented;
  };
  return {
    window, mediaDevices, messages, calls, tracks, click,
    setInputs(next) { inputs = next; },
    original,
    enumerations: () => enumerations,
    deviceChangeInstalled: () => !!listeners.devicechange,
    forceTrack: id => { trackId = id; },
    rejectWith: name => { reject = name; },
    delayCaptures: () => { delayed = []; return delayed; },
    releaseDelay: () => { delayed = null; },
    advance: ms => { now += ms; },
  };
}
const B = { mode: "device", deviceId: "B", label: "External microphone", groupId: "gB" };
const A = { mode: "device", deviceId: "A", label: "System microphone", groupId: "gA" };
const audio = () => ({ audio: { noiseSuppression: true, echoCancellation: false, deviceId: "default" }, video: false });

(async () => {
  const untrusted = harness("example.com");
  assert.equal(untrusted.window.__GPT_WRAPPER_MIC_ROUTE__, undefined);

  const h = harness();
  assert.equal(h.click("聽寫"), true, "Dictation waits for its initial policy");
  assert.equal(h.calls.length, 0, "injection never opens the microphone");
  h.window.__GPT_WRAPPER_MIC_ROUTE__.configure({ mode: "default" }, 1);
  const defaultConstraints = audio();
  h.click("聽寫");
  await h.mediaDevices.getUserMedia(defaultConstraints);
  assert.equal(h.calls.at(-1), defaultConstraints, "Windows Default is unchanged");
  assert.equal(h.enumerations(), 0);

  h.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 2);
  h.click("聽寫");
  const input = audio();
  await h.mediaDevices.getUserMedia(input);
  assert.equal(h.calls.at(-1).audio.deviceId.exact, "B");
  assert.equal(h.calls.at(-1).audio.noiseSuppression, true);
  assert.equal(h.calls.at(-1).audio.echoCancellation, false);
  assert.equal(input.audio.deviceId, "default", "original constraints unchanged");
  assert.equal(h.messages.at(-1).event, "captureVerified");
  await h.mediaDevices.getUserMedia({ audio: true });
  assert.equal(h.calls.at(-1).audio.deviceId.exact, "B", "second stream still exact");
  h.tracks.at(-1).end();
  assert.equal(h.messages.filter(msg => msg.event === "error").length, 0,
    "normal ended event must not invalidate the microphone");
  h.click("Start Voice");
  const voice = { audio: { deviceId: { ideal: "A" } }, video: false };
  await h.mediaDevices.getUserMedia(voice);
  assert.equal(h.calls.at(-1).audio.deviceId.exact, "B", "Voice Stream 1 uses the chosen input");
  assert.equal(h.calls.at(-1).audio.deviceId.ideal, undefined);
  await h.mediaDevices.getUserMedia({ audio: true });
  assert.equal(h.calls.at(-1).audio.deviceId.exact, "B", "Voice Stream 2 also uses the chosen input");
  assert.equal(h.messages.filter(msg => msg.event === "captureVerified").at(-1).purpose, "voice");
  assert.equal(voice.audio.deviceId.ideal, "A", "native Voice constraints are never mutated");
  h.click("聽寫"); h.click("Stop");
  await h.mediaDevices.getUserMedia(voice);
  assert.equal(h.calls.at(-1), voice, "Stop invalidates Dictation ticket");
  assert.equal(h.enumerations(), 0);
  assert.equal(h.deviceChangeInstalled(), false);

  const tabs = Array.from({ length: 3 }, () => harness());
  for (const tab of tabs) {
    tab.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 2);
    assert.equal(tab.enumerations(), 0, "new tabs cannot proactively enumerate");
    assert.equal(tab.deviceChangeInstalled(), false);
    tab.click("聽寫");
  }
  for (const tab of tabs) {
    await tab.mediaDevices.getUserMedia({ audio: true });
    assert.equal(tab.calls.at(-1).audio.deviceId.exact, "B");
  }
  // Failure on tab 2 does not touch tabs 1 or 3 or require re-selection.
  const failed = tabs[1];
  failed.click("聽寫");
  failed.rejectWith("NotFoundError");
  await assert.rejects(failed.mediaDevices.getUserMedia({ audio: true }), { name: "NotFoundError" });
  const errors = failed.messages.filter(msg => msg.event === "error");
  assert.equal(errors.length, 1);
  assert.equal(errors[0].attempt, failed.messages.filter(msg => msg.event === "attempt").at(-1).attempt);
  await assert.rejects(failed.mediaDevices.getUserMedia({ audio: true }), /capture failed/,
    "same failed attempt must not fall back to native Default");
  assert.equal(failed.calls.at(-1).audio.deviceId.exact, "B");
  failed.rejectWith(null);
  assert.equal(failed.click("聽寫"), false, "next click is permitted");
  await failed.mediaDevices.getUserMedia({ audio: true });
  assert.equal(failed.calls.at(-1).audio.deviceId.exact, "B");
  for (const good of [tabs[0], tabs[2]]) {
    good.click("聽寫");
    await good.mediaDevices.getUserMedia({ audio: true });
    assert.equal(good.messages.filter(msg => msg.event === "error").length, 0);
  }

  const permission = harness();
  permission.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  permission.click("聽寫");
  permission.rejectWith("NotAllowedError");
  await assert.rejects(permission.mediaDevices.getUserMedia({ audio: true }));
  assert.equal(permission.messages.at(-1).reason, "permission");
  permission.rejectWith(null);
  permission.click("聽寫");
  await permission.mediaDevices.getUserMedia({ audio: true });

  const mismatch = harness();
  mismatch.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  mismatch.click("聽寫");
  mismatch.forceTrack("A");
  await assert.rejects(mismatch.mediaDevices.getUserMedia({ audio: true }), /does not match/);
  assert.equal(mismatch.tracks.at(-1).stopped, true);
  assert.equal(mismatch.messages.at(-1).reason, "mismatch");
  mismatch.forceTrack(null);
  mismatch.click("聽寫");
  await mismatch.mediaDevices.getUserMedia({ audio: true });
  assert.equal(mismatch.calls.at(-1).audio.deviceId.exact, "B");

  const switching = harness();
  switching.window.__GPT_WRAPPER_MIC_ROUTE__.configure(A, 1);
  switching.click("聽寫");
  await switching.mediaDevices.getUserMedia({ audio: true });
  switching.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 2);
  assert.equal(switching.tracks.at(-1).stopped, true, "switching sources stops old track");
  await switching.mediaDevices.getUserMedia({ audio: true });
  assert.equal(switching.calls.at(-1).audio.deviceId.exact, "B");
  switching.window.__GPT_WRAPPER_MIC_ROUTE__.configure(A, 1);
  await switching.mediaDevices.getUserMedia({ audio: true });
  assert.equal(switching.calls.at(-1).audio.deviceId.exact, "B", "stale sync ignored");

  const lateError = harness();
  lateError.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  lateError.click("聽寫");
  const delayedError = lateError.delayCaptures();
  const afterStop = lateError.mediaDevices.getUserMedia({ audio: true });
  lateError.click("Stop");
  delayedError[0].reject("NotFoundError");
  await assert.rejects(afterStop, { name: "NotFoundError" });
  assert.equal(lateError.messages.filter(msg => msg.event === "error").length, 1,
    "failure after PTT Up still cancels any pending Send");

  const late = harness();
  late.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  late.click("聽寫");
  const pending = late.delayCaptures();
  const oldCapture = late.mediaDevices.getUserMedia({ audio: true });
  late.click("聽寫"); // new attempt supersedes the pending old one
  pending[0]();
  await assert.rejects(oldCapture, /does not match/);
  assert.equal(late.tracks[0].stopped, true);
  assert.equal(late.messages.filter(msg => msg.event === "error").length, 0,
    "an old async capture cannot error a new Dictation attempt");
  const defaultVoice = harness();
  assert.equal(defaultVoice.click("Start Voice"), true, "Voice must wait for initial preference");
  defaultVoice.window.__GPT_WRAPPER_MIC_ROUTE__.configure({ mode: "default" }, 1);
  defaultVoice.click("Start Voice");
  const nativeVoice = { audio: { deviceId: { ideal: "A" } }, video: false };
  const result = defaultVoice.mediaDevices.getUserMedia(nativeVoice);
  assert.equal(defaultVoice.calls.at(-1), nativeVoice, "Windows Default Voice preserves native constraints");
  await result;
  await defaultVoice.mediaDevices.getUserMedia({ audio: true });
  assert.equal(defaultVoice.calls.at(-1).audio, true, "both Default Voice captures remain native");
  defaultVoice.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 2);
  assert.equal(defaultVoice.tracks.every(track => track.stopped), true,
    "switching from Default to selected device stops classified Voice streams");

  const voiceFailure = harness();
  voiceFailure.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  voiceFailure.click("Start Voice");
  voiceFailure.rejectWith("NotFoundError");
  await assert.rejects(voiceFailure.mediaDevices.getUserMedia({ audio: true }), { name: "NotFoundError" });
  const voiceErrors = voiceFailure.messages.filter(msg => msg.event === "error");
  assert.equal(voiceErrors.length, 1);
  assert.equal(voiceErrors[0].purpose, "voice", "host can separate Voice from PTT failures");
  await assert.rejects(voiceFailure.mediaDevices.getUserMedia({ audio: true }), /capture failed/);
  voiceFailure.rejectWith(null);
  voiceFailure.click("Start Voice");
  await voiceFailure.mediaDevices.getUserMedia({ audio: true });
  assert.equal(voiceFailure.calls.at(-1).audio.deviceId.exact, "B",
    "Voice can retry without Settings unlock");

  const voiceMismatch = harness();
  voiceMismatch.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  voiceMismatch.click("Start Voice");
  voiceMismatch.forceTrack("A");
  await assert.rejects(voiceMismatch.mediaDevices.getUserMedia({ audio: true }), /does not match/);
  assert.equal(voiceMismatch.tracks.at(-1).stopped, true);
  assert.equal(voiceMismatch.messages.filter(msg => msg.event === "error").at(-1).purpose, "voice");
  voiceMismatch.forceTrack(null);
  voiceMismatch.click("Start Voice");
  await voiceMismatch.mediaDevices.getUserMedia({ audio: true });

  const bounded = harness();
  bounded.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  bounded.click("Start Voice");
  bounded.advance(8001);
  const unrelated = { audio: true, video: false };
  await bounded.mediaDevices.getUserMedia(unrelated);
  assert.equal(bounded.calls.at(-1), unrelated,
    "expired Voice gesture cannot rewrite unrelated capture");
  bounded.click("Start Voice");
  await bounded.mediaDevices.getUserMedia({ audio: true });
  bounded.advance(2000);
  await bounded.mediaDevices.getUserMedia({ audio: true });
  assert.equal(bounded.calls.at(-1).audio.deviceId.exact, "B",
    "Voice rolling window includes its second stream");
  bounded.advance(13000);
  await bounded.mediaDevices.getUserMedia(unrelated);
  assert.equal(bounded.calls.at(-1), unrelated,
    "Voice classification closes after the bounded follow-on window");
  bounded.click("聽寫");
  await bounded.mediaDevices.getUserMedia({ audio: true });
  assert.equal(bounded.calls.at(-1).audio.deviceId.exact, "B",
    "Dictation still works after Voice expires");

  const voiceSwitch = harness();
  voiceSwitch.window.__GPT_WRAPPER_MIC_ROUTE__.configure(A, 1);
  voiceSwitch.click("Start Voice");
  const delayedVoice = voiceSwitch.delayCaptures();
  const inFlight = voiceSwitch.mediaDevices.getUserMedia({ audio: true });
  voiceSwitch.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 2);
  delayedVoice[0]();
  await assert.rejects(inFlight, /does not match/);
  assert.equal(voiceSwitch.tracks.at(-1).stopped, true, "in-flight Voice must not keep old device");
  assert.equal(voiceSwitch.messages.filter(msg => msg.event === "error").length, 0,
    "stale Voice completion must not error a newer preference");
  voiceSwitch.releaseDelay();
  voiceSwitch.click("Start Voice");
  await voiceSwitch.mediaDevices.getUserMedia({ audio: true });
  assert.equal(voiceSwitch.calls.at(-1).audio.deviceId.exact, "B");

  const voiceStopped = harness();
  voiceStopped.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  voiceStopped.click("Start Voice");
  await voiceStopped.mediaDevices.getUserMedia({ audio: true });
  voiceStopped.window.__GPT_WRAPPER_MIC_ROUTE__.configure({ mode: "default" }, 2);
  assert.equal(voiceStopped.tracks.at(-1).stopped, true);
  const fallbackNative = { audio: true };
  voiceStopped.click("Start Voice");
  await voiceStopped.mediaDevices.getUserMedia(fallbackNative);
  assert.equal(voiceStopped.calls.at(-1), fallbackNative);

  const racing = harness();
  racing.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  racing.click("聽寫");
  const racingPromises = racing.delayCaptures();
  const oldDictation = racing.mediaDevices.getUserMedia({ audio: true });
  racing.click("Start Voice");
  racingPromises[0].reject("NotFoundError");
  await assert.rejects(oldDictation, { name: "NotFoundError" });
  assert.equal(racing.messages.filter(msg => msg.event === "error").length, 0,
    "old Dictation failure cannot cancel a newer Voice gesture");


  const leaseId = "11111111-1111-4111-8111-111111111111";
  const leaseTab = harness();
  leaseTab.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  leaseTab.setInputs([{ kind: "audioinput", deviceId: "origin-cable", label: "CABLE Output (VB-Audio Virtual Cable)" }]);
  leaseTab.click("Start Voice");
  await leaseTab.mediaDevices.getUserMedia({ audio: true });
  const ongoingVoice = leaseTab.tracks.at(-1);
  await leaseTab.window.__GPT_WRAPPER_MIC_ROUTE__.installDictationLease(leaseId);
  assert.equal(ongoingVoice.stopped, false, "installing a lease leaves Voice tracks alone");
  assert.equal(leaseTab.calls.length, 1, "install only enumerates; no capture before PTT Start");
  assert.equal(leaseTab.messages.at(-1).event, "applied");
  leaseTab.click("聽寫");
  await leaseTab.mediaDevices.getUserMedia({ audio: true });
  const deviceTrack = leaseTab.tracks.at(-1);
  assert.equal(leaseTab.calls.at(-1).audio.deviceId.exact, "origin-cable");
  assert(leaseTab.messages.some(m => m.kind === "gpt-wrapper-device-route" && m.event === "captureVerified" && m.lease === leaseId));
  leaseTab.window.__GPT_WRAPPER_MIC_ROUTE__.revokeDictationLease(leaseId);
  assert.equal(deviceTrack.stopped, true);
  assert.equal(ongoingVoice.stopped, false, "revoke only stops this lease's Dictation tracks");
  leaseTab.click("聽寫");
  await leaseTab.mediaDevices.getUserMedia({ audio: true });
  assert.equal(leaseTab.calls.at(-1).audio.deviceId.exact, "B", "preference restored without a global policy change");

  const lateLease = harness();
  lateLease.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  lateLease.setInputs([{ kind: "audioinput", deviceId: "origin-cable", label: "CABLE Output (VB-Audio Virtual Cable)" }]);
  await lateLease.window.__GPT_WRAPPER_MIC_ROUTE__.installDictationLease(leaseId);
  lateLease.click("聽寫");
  const delayedLeaseCaptures = lateLease.delayCaptures();
  const leasedInFlight = lateLease.mediaDevices.getUserMedia({ audio: true });
  lateLease.window.__GPT_WRAPPER_MIC_ROUTE__.revokeDictationLease(leaseId);
  delayedLeaseCaptures[0]();
  await assert.rejects(leasedInFlight, { name: "NotReadableError" });
  assert.equal(lateLease.tracks.at(-1).stopped, true);
  assert(!lateLease.messages.some(m => m.kind === "gpt-wrapper-device-route" && m.event === "captureVerified"));

  const absentCable = harness();
  absentCable.window.__GPT_WRAPPER_MIC_ROUTE__.configure(B, 1);
  await absentCable.window.__GPT_WRAPPER_MIC_ROUTE__.installDictationLease(leaseId);
  assert.equal(absentCable.calls.length, 0);
  assert(absentCable.messages.some(m => m.kind === "gpt-wrapper-device-route" && m.event === "failed"));

  console.log("Microphone routing tests passed: multiple tabs, native Default, dual-stream Voice, retries, mismatch, bounded tickets, preference changes and stale requests.");
})().catch(error => { console.error(error); process.exitCode = 1; });
