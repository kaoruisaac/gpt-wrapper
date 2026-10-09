// Read-only inventory for the Settings microphone picker.
// Always runs in the ChatGPT WebView origin/profile, never in the Settings window.
(() => {
  if (window.top !== window ||
      location.protocol !== "https:" ||
      !["chatgpt.com", "chat.openai.com"].includes(location.hostname) ||
      window.__GPT_WRAPPER_MIC_INVENTORY__) return;

  const post = (payload) => {
    try {
      window.chrome?.webview?.postMessage(JSON.stringify({
        kind: "gpt-wrapper-microphone-inventory", ...payload,
      }));
    } catch { /* No website behavior should depend on the inventory bridge. */ }
  };

  window.__GPT_WRAPPER_MIC_INVENTORY__ = {
    async query(requestId) {
      if (typeof requestId !== "string" || requestId.length > 100) return;
      const media = navigator.mediaDevices;
      if (!media?.enumerateDevices) {
        post({ requestId, error: "Audio device enumeration is unsupported." });
        return;
      }
      try {
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
    },
  };
})();
