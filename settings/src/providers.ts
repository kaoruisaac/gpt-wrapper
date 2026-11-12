// Provider metadata for the Settings window. Kept free of DOM and Solid
// imports so it can be unit-tested directly with Node.

export type ProviderId = "chatgpt" | "claude";

export type ProviderChoice = {
  id: ProviderId;
  letter: string;
  name: string;
  builtin: string;
  home: string;
  urlHint: string;
};

export const PROVIDER_CHOICES: readonly ProviderChoice[] = [
  {
    id: "chatgpt", letter: "G", name: "ChatGPT", builtin: "chatgpt.com",
    home: "https://chatgpt.com", urlHint: "https://chatgpt.com/g/…",
  },
  {
    id: "claude", letter: "C", name: "Claude", builtin: "claude.ai",
    home: "https://claude.ai", urlHint: "https://claude.ai/project/…",
  },
];

export function providerChoice(id: ProviderId): ProviderChoice {
  return PROVIDER_CHOICES.find((choice) => choice.id === id) ?? PROVIDER_CHOICES[0];
}

export type ProjectBookmark = { id: string; name: string; url: string; messagePrefix: string };
export type ProjectList = { defaultProjectId: string | null; projects: ProjectBookmark[] };
export type ProjectSettings = { chatgpt: ProjectList; claude: ProjectList };

export function emptyProjectSettings(): ProjectSettings {
  return {
    chatgpt: { defaultProjectId: null, projects: [] },
    claude: { defaultProjectId: null, projects: [] },
  };
}

const isClaudeHost = (host: string) => host === "claude.ai" || host.endsWith(".claude.ai");

// Mirrors the Rust validation (which remains authoritative): Claude bookmarks
// must be https://claude.ai pages, and a Claude URL is never a ChatGPT bookmark.
export function bookmarkUrlError(provider: ProviderId, value: string): string {
  let url: URL;
  try {
    url = new URL(value.trim());
  } catch {
    return provider === "claude"
      ? "Claude 書籤只接受 https://claude.ai/ 的網址。"
      : "請輸入有效的 http:// 或 https:// URL。";
  }
  if (provider === "claude") {
    return url.protocol === "https:" && url.hostname === "claude.ai" && !url.port
      ? ""
      : "Claude 書籤只接受 https://claude.ai/ 的網址。";
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    return "請輸入有效的 http:// 或 https:// URL。";
  }
  if (isClaudeHost(url.hostname.toLowerCase())) {
    return "這是 Claude 網址，請改加入 Claude（C）的專案書籤。";
  }
  return "";
}
