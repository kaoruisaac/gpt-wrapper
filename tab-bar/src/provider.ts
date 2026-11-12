// Provider identity for the Tab Bar. Rust sends stable IDs; only the UI maps
// them to the G / C letters. Keep this module free of DOM and Solid imports so
// it can be unit-tested directly with Node.

export type ProviderId = "chatgpt" | "claude";

export type ProviderInfo = {
  id: ProviderId;
  letter: string;
  name: string;
};

export const PROVIDERS: readonly ProviderInfo[] = [
  { id: "chatgpt", letter: "G", name: "ChatGPT" },
  { id: "claude", letter: "C", name: "Claude" },
];

export function isProviderId(value: unknown): value is ProviderId {
  return value === "chatgpt" || value === "claude";
}

export function providerInfo(id: ProviderId): ProviderInfo {
  return PROVIDERS.find((provider) => provider.id === id) ?? PROVIDERS[0];
}

export type SwitcherButtonState = {
  pressed: boolean;
  pending: boolean;
  disabled: boolean;
};

export function switcherButtonState(
  id: ProviderId,
  active: ProviderId,
  pending: ProviderId | null,
  busy: boolean,
): SwitcherButtonState {
  return {
    pressed: id === active,
    pending: pending === id,
    // While any tab command or switch is running, both buttons are inert so
    // repeated clicks cannot race each other.
    disabled: busy || pending !== null,
  };
}

// A drag or optimistic reorder that started on one provider must never be
// committed against another provider's list.
export function providerChanged(
  previous: { activeProvider: ProviderId },
  next: { activeProvider: ProviderId },
): boolean {
  return previous.activeProvider !== next.activeProvider;
}

export function errorMessage(error: unknown): string {
  if (typeof error === "string" && error.trim()) return error.trim();
  if (error instanceof Error && error.message.trim()) return error.message.trim();
  return "操作失敗，請再試一次。";
}
