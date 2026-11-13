// Display wording only (never a security decision): the Settings WebView is
// WebView2 on Windows and WKWebView on macOS.
export const IS_MAC = typeof navigator !== "undefined" && /Macintosh|Mac OS X/.test(navigator.userAgent);
export const SYSTEM_DEFAULT_LABEL = IS_MAC ? "macOS 系統預設" : "Windows Default";
export const SYSTEM_DEFAULT_HINT = IS_MAC ? "依 macOS「聲音 → 輸入」的預設裝置" : "依 Windows 系統預設裝置";
