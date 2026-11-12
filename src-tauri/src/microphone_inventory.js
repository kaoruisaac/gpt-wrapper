// Inventory for the Settings microphone picker. Runs in a provider tab's own
// origin (device IDs are per origin), never in the Settings window.
(() => {
  // Rust pins the tab's provider; tests leave it unpinned.
  const PINNED = /*GPT_WRAPPER_PROVIDER*/null;
  const HOSTS = { chatgpt: ["chatgpt.com", "chat.openai.com"], claude: ["claude.ai"] };
  const providerId = Object.keys(HOSTS).find((id) => HOSTS[id].includes(location.hostname));
  if (window.top !== window ||
      location.protocol !== "https:" || !providerId ||
      (PINNED !== null && PINNED !== providerId) ||
      window.__GPT_WRAPPER_MIC_INVENTORY__) return;

  const post = (payload) => {
    try {
      window.chrome?.webview?.postMessage(JSON.stringify({
        kind: "gpt-wrapper-microphone-inventory", ...payload,
      }));
    } catch { /* No website behavior should depend on the inventory bridge. */ }
  };

  window.__GPT_WRAPPER_MIC_INVENTORY__ = {
    // Listing never opens the microphone.
    query(requestId) { return list(requestId, false); },
    // Only an explicit Settings "allow" action asks this site for microphone
    // permission, so device names become visible. The stream is closed at once.
    requestAccess(requestId) { return list(requestId, true); },
  };

  async function list(requestId, requestAccess) {
      if (typeof requestId !== "string" || requestId.length > 100) return;
      const media = navigator.mediaDevices;
      if (!media?.enumerateDevices) {
        post({ requestId, error: "Audio device enumeration is unsupported." });
        return;
      }
      try {
        if (requestAccess) {
          const stream = await media.getUserMedia({ audio: true });
          for (const track of stream?.getTracks?.() || []) {
            try { track.stop(); } catch { /* keep closing */ }
          }
        }
        const raw = await media.enumerateDevices();
        // Do not offer pseudo default/communications entries as fixed hardware IDs.
        const devices = raw.filter((device) =>
          device.kind === "audioinput" && device.deviceId &&
          device.deviceId !== "default" && device.deviceId !== "communications"
        ).map((device) => ({
          deviceId: device.deviceId,
          groupId: device.groupId || "",
          label: device.label || "",
        }));
        post({ requestId, devices });
      } catch (error) {
        post({ requestId, error: error?.name || "Could not enumerate microphones." });
      }
  }
})();
