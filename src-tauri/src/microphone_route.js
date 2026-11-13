// Trusted provider top-frame audio routing for Dictation (and ChatGPT Voice
// Mode). Device enumeration for Settings belongs to microphone_inventory.js.
(() => {
  // Rust pins the tab's provider here; an unpinned copy (tests) derives it
  // from the trusted host. A pinned tab never routes another provider's page.
  const PINNED = /*GPT_WRAPPER_PROVIDER*/null;
  const PROVIDERS = {
    chatgpt: {
      name: "ChatGPT", hosts: ["chatgpt.com", "chat.openai.com"],
      control: "button[aria-label]",
      dictation: ["聽寫", "開始聽寫", "开始听写", "Start dictation", "Dictate"],
      voice: ["開始語音對話", "Start Voice"],
      stop: ["停止", "Stop"],
    },
    // Claude Voice Mode capture is not identified, so only Dictation is
    // classified; any other capture keeps the site's native behavior.
    claude: {
      name: "Claude", hosts: ["claude.ai"],
      control: "[aria-label]",
      dictation: ["口述輸入"],
      voice: [],
      stop: ["完成聽寫"],
    },
  };
  const providerId = Object.keys(PROVIDERS)
    .find((id) => PROVIDERS[id].hosts.includes(location.hostname));
  if (window !== window.top || location.protocol !== "https:" || !providerId ||
      (PINNED !== null && PINNED !== providerId) ||
      window.__GPT_WRAPPER_MIC_ROUTE__) return;
  const site = PROVIDERS[providerId];
  const media = navigator.mediaDevices;
  if (!media || typeof media.getUserMedia !== "function") return;
  const nativeCapture = media.getUserMedia;
  // A new page must receive its preference before starting either mode.
  let policy = null, epoch = -1, intent = null, lastTicket = null, sequence = 0;
  let deviceLease = null;
  const devicePost = (event, lease, attempt = "") => {
    try { (window.chrome?.webview ?? window.webkit?.messageHandlers?.gptWrapper)?.postMessage(JSON.stringify({
      kind: "gpt-wrapper-device-route", event, lease: lease.id, attempt,
    })); } catch { /* Host watchdog remains closed without evidence. */ }
  };
  const revokeDevice = (id) => {
    if (!deviceLease || deviceLease.id !== id) return;
    const old = deviceLease; deviceLease = null;
    old.revoked = true;
    for (const track of old.tracks) { try { track.stop(); } catch {} managedTracks.delete(track); }
    devicePost("revoked", old);
  };
  // Only streams acquired from a recognized provider interaction are managed.
  const managedTracks = new Map();
  let toast = null;

  const notify = (event, reason = "", ticket = null) => {
    try {
      (window.chrome?.webview ?? window.webkit?.messageHandlers?.gptWrapper)?.postMessage(JSON.stringify({
        kind: "gpt-wrapper-microphone-route", event, epoch, reason,
        attempt: ticket?.id || "", purpose: ticket?.purpose || "",
      }));
    } catch { /* Diagnostics must not affect native capture. */ }
  };
  const reasonText = {
    unavailable: "選定的麥克風目前無法使用。可直接重試，或到 Settings → General 檢查裝置。",
    permission: `${site.name} 麥克風權限被拒絕。請檢查網站權限後重試。`,
    confirm: `${site.name} 無法唯一辨識所選麥克風。請到 Settings → General，選擇 ${site.name} 來源後重新確認麥克風。`,
    mismatch: "取得的音源與所選麥克風不同，已停止該次收音。",
    pending: "麥克風設定同步中，請稍後再試。",
    capture: "無法啟用選定麥克風。可重試或到 Settings → General 重新整理。",
    restart: "麥克風來源已變更，原收音已停止。請重新啟動 Voice Mode。",
  };
  const showError = (reason) => {
    const content = reasonText[reason] || reasonText.capture;
    const display = () => {
      if (!document.body) return;
      if (!toast) {
        toast = document.createElement("div");
        toast.setAttribute("role", "alert");
        toast.setAttribute("data-gpt-wrapper-mic-error", "");
        Object.assign(toast.style, {
          position: "fixed", right: "16px", bottom: "24px", zIndex: "2147483647",
          maxWidth: "340px", borderRadius: "10px", border: "1px solid #f4b4ae",
          padding: "12px 14px", background: "#fff", color: "#a12018",
          boxShadow: "0 8px 25px #0002", font: "13px/1.5 system-ui",
        });
        const body = document.createElement("p");
        body.style.margin = "0 0 8px";
        body.setAttribute("data-error-message", "");
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = "開啟 Settings";
        button.style.cssText = "border:1px solid #d0d0d0;background:#fff;border-radius:6px;padding:5px 9px;cursor:pointer;color:#202123";
        button.addEventListener("click", () => notify("openSettings"));
        toast.append(body, button);
      }
      toast.querySelector("[data-error-message]").textContent = content;
      if (!toast.isConnected) document.body.append(toast);
    };
    if (document.body) display();
    else window.addEventListener("DOMContentLoaded", display, { once: true });
  };
  const hideError = () => { if (toast?.isConnected) toast.remove(); };
  // Dictation can fail after a PTT Up. Voice errors must remain local to the
  // current tab and never be forwarded to the PTT cancellation path.
  const fail = (reason, ticket) => {
    if (!ticket || ticket !== lastTicket || ticket.epoch !== epoch || ticket.failed) return;
    if (ticket.purpose === "voice" && ticket !== intent) return;
    ticket.failed = true;
    if (ticket.lease) devicePost("failed", ticket.lease, ticket.id);
    notify("error", reason, ticket);
    showError(reason);
  };
  const labels = { dictation: site.dictation, voice: site.voice };
  const newTicket = (purpose) => {
    const now = Date.now();
    return {
      purpose, epoch, id: `${now}-${++sequence}`,
      expires: now + 8000, voiceDeadline: purpose === "voice" ? now + 45000 : 0,
      active: false, failed: false,
    };
  };
  // Only recognized buttons open a capture classification window.
  document.addEventListener("click", (event) => {
    const button = event.target?.closest?.(site.control);
    const label = button?.getAttribute("aria-label")?.trim();
    const purpose = labels.voice.includes(label) ? "voice"
      : labels.dictation.includes(label) ? "dictation" : null;
    if (!purpose) {
      if (site.stop.includes(label)) {
        // Keep the last Dictation attempt to associate a delayed failure
        // after PTT Up, but stop classifying new requests.
        intent = null;
      }
      return;
    }
    intent = null;
    lastTicket = null;
    if (!policy || policy.pending || policy.unresolved) {
      event.preventDefault();
      event.stopImmediatePropagation();
      showError(policy?.unresolved ? "confirm" : "pending");
      return;
    }
    hideError();
    intent = newTicket(purpose);
    if (purpose === "dictation" && deviceLease?.deviceId && !deviceLease.revoked) {
      intent.lease = deviceLease;
      devicePost("attempt", deviceLease, intent.id);
    }
    if (purpose === "voice" && deviceLease) revokeDevice(deviceLease.id);
    lastTicket = intent;
    if (purpose === "voice" || policy.mode === "device") notify("attempt", "", intent);
  }, true);

  const registerTracks = (stream, purpose) => {
    for (const track of stream?.getAudioTracks?.() || []) {
      managedTracks.set(track, purpose);
      track.addEventListener?.("ended", () => { managedTracks.delete(track); }, { once: true });
    }
  };
  const stopStream = (stream) => {
    for (const track of stream?.getTracks?.() || []) {
      try { track.stop(); } catch { /* stop remaining tracks */ }
      managedTracks.delete(track);
    }
  };
  const classification = () => {
    const ticket = intent;
    if (!ticket) return null;
    if (ticket.purpose === "voice" && Date.now() > ticket.expires) {
      // Voice has a short, rolling multi-stream window, with a hard upper
      // bound. Do not guess Voice end state from unrelated DOM selectors.
      intent = null;
      if (lastTicket === ticket) lastTicket = null;
      return null;
    }
    return ticket;
  };

  media.getUserMedia = function (...args) {
    const audio = args[0]?.audio;
    const ticket = audio && classification();
    if (!audio || !ticket || !policy)
      return Reflect.apply(nativeCapture, this, args);

    const lease = ticket.lease;
    if (lease && (lease !== deviceLease || lease.revoked)) {
      return Promise.reject(new DOMException("Dictation lease revoked", "NotReadableError"));
    }
    if (!lease && policy.mode === "default") {
      // Return the original Promise and constraints unchanged. Observe only
      // classified tracks to stop them if Settings changes mid-session.
      const captureEpoch = epoch;
      const captured = Reflect.apply(nativeCapture, this, args);
      Promise.resolve(captured).then(stream => {
        if (captureEpoch !== epoch) stopStream(stream);
        else registerTracks(stream, ticket.purpose);
      }, () => { /* Native Promise owns its own rejection. */ });
      return captured;
    }
    if (ticket.failed) {
      return Promise.reject(new DOMException("Selected microphone capture failed", "NotReadableError"));
    }
    if (ticket.epoch !== epoch || Date.now() > ticket.expires) {
      fail("capture", ticket);
      return Promise.reject(new DOMException("Selected microphone request expired", "NotReadableError"));
    }
    ticket.active = true;
    if (ticket.purpose === "dictation") {
      // Retain the existing multi-capture Dictation session behavior.
      ticket.expires = Number.POSITIVE_INFINITY;
    } else {
      // ChatGPT Voice has been observed requesting two initial streams.
      // Allow a short follow-on interval, never a permanent voice ticket.
      ticket.expires = Math.min(ticket.voiceDeadline, Date.now() + 12000);
    }
    const selected = lease ? lease.deviceId : policy.deviceId;
    const original = typeof audio === "object" ? audio : {};
    const constraints = { ...args[0], audio: { ...original, deviceId: { exact: selected } } };
    let captured;
    try { captured = Reflect.apply(nativeCapture, this, [constraints, ...args.slice(1)]); }
    catch (error) {
      fail(["NotAllowedError", "SecurityError"].includes(error?.name) ? "permission" : "capture", ticket);
      throw error;
    }
    return Promise.resolve(captured).then(stream => {
      const tracks = stream?.getAudioTracks?.() || [];
      if (ticket !== intent || ticket.epoch !== epoch || ticket.failed ||
          (lease && (lease !== deviceLease || lease.revoked)) ||
          !tracks.length || tracks.some(track => track.getSettings?.()?.deviceId !== selected)) {
        stopStream(stream);
        if (ticket === intent && ticket.epoch === epoch) fail("mismatch", ticket);
        throw new DOMException("Selected microphone does not match", "NotReadableError");
      }
      registerTracks(stream, ticket.purpose);
      if (lease) {
        for (const track of tracks) {
          lease.tracks.add(track);
          track.addEventListener?.("ended", () => {
            lease.tracks.delete(track);
            if (deviceLease === lease && !lease.revoked && !lease.tracks.size) devicePost("failed", lease, ticket.id);
          }, { once: true });
        }
        devicePost("captureVerified", lease, ticket.id);
      }
      notify("captureVerified", "", ticket);
      return stream;
    }, error => {
      fail(["NotAllowedError", "SecurityError"].includes(error?.name) ? "permission" : "capture", ticket);
      throw error;
    });
  };

  window.__GPT_WRAPPER_MIC_ROUTE__ = {
    async installDictationLease(id) {
      if (typeof id !== "string" || id.length !== 36 || deviceLease) return;
      const lease = { id, deviceId: null, revoked: false, tracks: new Set() };
      deviceLease = lease;
      try {
        const devices = await media.enumerateDevices();
        const cables = devices.filter(d => d.kind === "audioinput" && d.deviceId &&
          !["default", "communications"].includes(d.deviceId) &&
          /^CABLE Output \(/.test(d.label) && d.label.includes("VB-Audio"));
        if (deviceLease !== lease || lease.revoked) return;
        if (cables.length !== 1) throw new Error("cable_missing_or_ambiguous");
        lease.deviceId = cables[0].deviceId;
        devicePost("applied", lease);
      } catch {
        if (deviceLease === lease) { devicePost("failed", lease); showError("unavailable"); }
      }
    },
    revokeDictationLease: revokeDevice,
    configure(next, nextEpoch) {
      if (!Number.isSafeInteger(nextEpoch) || nextEpoch < epoch ||
          !next || !["default", "device"].includes(next.mode) ||
          (next.mode === "device" && (!next.label || (!next.deviceId && !next.resolve)))) return;
      if (nextEpoch === epoch && policy) {
        // A failed origin match is retried on each re-sync (e.g. after the
        // user grants this site microphone permission).
        if (policy.unresolved && next.resolve) { resolvePolicy(next, epoch); return; }
        if (policy.pending) return;
        if (policy.mode === next.mode &&
            (next.mode === "default" || next.resolve || policy.deviceId === next.deviceId)) notify("ready");
        return;
      }
      if (deviceLease) revokeDevice(deviceLease.id);
      const oldId = policy?.mode === "device" ? policy.deviceId : null;
      const newId = next.mode === "device" ? next.deviceId : null;
      const changed = !!policy && oldId !== newId;
      const interruptedVoice = changed &&
        (intent?.purpose === "voice" && intent.active ||
         Array.from(managedTracks.values()).includes("voice"));
      if (changed) {
        for (const track of managedTracks.keys()) {
          try { track.stop(); } catch { /* ignore */ }
        }
        managedTracks.clear();
      }
      // Preserve the proven Dictation A -> B behavior while invalidating
      // in-flight requests from the old epoch. Voice must be restarted.
      const continuing = intent?.purpose === "dictation" && intent.active &&
        !intent.failed && next.mode === "device";
      epoch = nextEpoch;
      if (next.resolve) {
        policy = Object.freeze({ ...next, deviceId: null, pending: true });
        intent = null;
        lastTicket = null;
        hideError();
        if (interruptedVoice) showError("restart");
        resolvePolicy(next, nextEpoch);
        return;
      }
      policy = Object.freeze({ ...next });
      intent = continuing ? { ...intent, epoch, failed: false } : null;
      lastTicket = intent;
      hideError();
      if (interruptedVoice) showError("restart");
      notify("ready");
    },
    showError,
  };

  // The shared choice came from another provider's origin, whose device IDs
  // are not valid here. Match the same physical device by its exact label in
  // this origin's own inventory; never pick the first device or fall back to
  // Windows Default when that is impossible.
  let resolving = 0;
  async function resolvePolicy(next, resolveEpoch) {
    const attempt = ++resolving;
    policy = Object.freeze({ ...next, deviceId: null, pending: true });
    let reason = "missing", deviceId = null;
    try {
      const inputs = (await media.enumerateDevices()).filter((d) =>
        d.kind === "audioinput" && d.deviceId &&
        !["default", "communications"].includes(d.deviceId));
      const matches = inputs.filter((d) => d.label === next.label);
      if (inputs.length && inputs.every((d) => !d.label)) reason = "permission";
      else if (matches.length > 1) reason = "ambiguous";
      else if (matches.length === 1) deviceId = matches[0].deviceId;
    } catch {
      reason = "permission";
    }
    if (attempt !== resolving || resolveEpoch !== epoch) return;
    if (deviceId) {
      policy = Object.freeze({ mode: "device", label: next.label, groupId: "", deviceId });
      notify("ready");
    } else {
      policy = Object.freeze({ ...next, deviceId: null, unresolved: true });
      notify("unresolved", reason);
    }
  }
})();
