// Opt-in diagnostic only. Never enabled by default or in release builds.
// It does not request audio, change constraints, select devices, or retain audio.
(() => {
  if (location.protocol !== "https:" ||
      !["chatgpt.com", "chat.openai.com"].includes(location.hostname)) return;
  const media = navigator.mediaDevices;
  if (!media || typeof media.getUserMedia !== "function" ||
      window.__GPT_WRAPPER_MICROPHONE_POC__) return;

  const original = media.getUserMedia;
  const topFrame = window.top === window;
  const sendToHost = (detail) => {
    try {
      window.chrome?.webview?.postMessage(JSON.stringify({
        kind: "gpt-wrapper-microphone-probe", ...detail,
      }));
    } catch { /* diagnostics must never disrupt ChatGPT */ }
  };
  const send = (detail) => {
    const event = { ...detail, frame: topFrame ? "top" : "child" };
    if (topFrame) {
      sendToHost(event);
    } else {
      // WebView2 top-level WebMessageReceived doesn't necessarily receive child-frame messages.
      try { window.top.postMessage({ __gptWrapperMicProbe: event }, location.origin); }
      catch { /* no effect on ChatGPT */ }
    }
  };
  if (topFrame) window.addEventListener("message", (event) => {
    if (event.origin === location.origin && event.data?.__gptWrapperMicProbe) {
      sendToHost(event.data.__gptWrapperMicProbe);
    }
  });
  let lastGesture = "unknown";
  let gestureAt = 0;
  const labels = {
    dictation: ["聽寫", "開始聽寫", "开始听写", "Start dictation", "Dictate"],
    voice: ["開始語音對話", "Start Voice"],
  };
  // Observe real DOM events, not a guessed API stack or a global audio policy.
  document.addEventListener("click", (event) => {
    const button = event.target?.closest?.("button[aria-label]");
    const label = button?.getAttribute("aria-label")?.trim();
    if (labels.dictation.includes(label)) lastGesture = "dictation";
    else if (labels.voice.includes(label)) lastGesture = "voice";
    else return;
    gestureAt = Date.now();
    send({ phase: "gesture", gesture: lastGesture });
  }, true);

  function identifyTrack(stream, captureId, gesture) {
    const track = stream?.getAudioTracks?.()[0];
    if (!track) {
      send({ phase: "fulfilled", captureId, gesture, audioTrack: false });
      return;
    }
    const id = track.getSettings?.().deviceId;
    // Enumerate only after ChatGPT itself obtained permission, without opening a second stream.
    Promise.resolve().then(() => media.enumerateDevices?.()).then((devices) => {
      const inputs = (devices || []).filter((device) => device.kind === "audioinput");
      const match = inputs.find((device) => !!id && device.deviceId === id);
      send({
        phase: "fulfilled", captureId, gesture, audioTrack: true,
        matchedDeviceLabel: match?.label || null,
        inputLabels: inputs.map((d) => d.label || "(permission required)"),
        idMatchedEnumeration: !!match,
      });
    }).catch(() => {
      send({ phase: "fulfilled", captureId, gesture, audioTrack: true,
             matchedDeviceLabel: null, enumerationFailed: true });
    });
  }

  let captureId = 0;
  function observedGetUserMedia(...args) {
    const constraints = args[0];
    const gesture = Date.now() - gestureAt <= 5000 ? lastGesture : "unknown";
    const id = ++captureId;
    send({
      phase: "requested", captureId: id, gesture,
      audio: !!constraints?.audio, video: !!constraints?.video,
      hasExplicitDeviceId: !!(constraints?.audio &&
        typeof constraints.audio === "object" &&
        Object.prototype.hasOwnProperty.call(constraints.audio, "deviceId")),
    });
    // Same constraints, same native Promise. No fake stream or audio interception.
    const promise = Reflect.apply(original, this, args);
    Promise.resolve(promise).then(
      (stream) => { void identifyTrack(stream, id, gesture); },
      (error) => { send({ phase: "rejected", captureId: id, gesture,
                         reason: error?.name || "unknown" }); },
    );
    return promise;
  }
  try {
    media.getUserMedia = observedGetUserMedia;
    if (media.getUserMedia !== observedGetUserMedia) {
      send({ phase: "setup-failed", reason: "mediaDevices.getUserMedia not writable" });
      return;
    }
  } catch {
    send({ phase: "setup-failed", reason: "mediaDevices.getUserMedia replacement rejected" });
    return;
  }
  window.__GPT_WRAPPER_MICROPHONE_POC__ = true;
  send({ phase: "ready" });
})();
