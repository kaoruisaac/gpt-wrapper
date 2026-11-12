import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { FiPlus } from "solid-icons/fi";
import {
  PROVIDER_CHOICES,
  type ProviderId,
  bookmarkUrlError,
  emptyProjectSettings,
  providerChoice,
  type ProjectSettings,
} from "./providers";

type Draft = { id: string | null; name: string; url: string; messagePrefix: string };

const emptyDraft = (): Draft => ({ id: null, name: "", url: "", messagePrefix: "" });

function validationError(provider: ProviderId, draft: Draft): string {
  if (!draft.name.trim()) return "請輸入 Project 名稱。";
  if (!draft.url.trim()) return "請輸入 Project URL。";
  return bookmarkUrlError(provider, draft.url);
}

// Each provider has its own bookmarks, Default Project and Message Prefixes.
// The G/C selector only chooses which list is edited here; it never changes
// the provider shown in the main window.
export default function ProjectBookmarks() {
  const [settings, setSettings] = createSignal<ProjectSettings>(emptyProjectSettings());
  const [provider, setProvider] = createSignal<ProviderId>("chatgpt");
  const [loading, setLoading] = createSignal(true);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const [draft, setDraft] = createSignal<Draft | null>(null);
  const list = () => settings()[provider()];
  const choice = () => providerChoice(provider());

  const reloadSettings = async () => {
    if (busy()) return;
    try {
      setSettings(await invoke<ProjectSettings>("get_project_settings"));
    } catch (cause) {
      console.error("[gpt-wrapper] failed to read project settings:", cause);
      setError("無法讀取專案書籤。");
    } finally {
      setLoading(false);
    }
  };

  onMount(() => {
    void reloadSettings();
    // Background Ctrl+. may change Default while Settings is behind another window.
    // Reload when Settings receives focus rather than trusting its original onMount snapshot.
    window.addEventListener("focus", reloadSettings);
    onCleanup(() => window.removeEventListener("focus", reloadSettings));
  });

  const mutate = async (command: string, args: Record<string, unknown>) => {
    setBusy(true);
    setError("");
    try {
      // Rust validates the list named here; it never infers it from a tab.
      const latest = await invoke<ProjectSettings>(command, { provider: provider(), ...args });
      setSettings(latest);
      return true;
    } catch (cause) {
      console.error(`[gpt-wrapper] ${command} failed:`, cause);
      setError(typeof cause === "string" ? cause : "儲存失敗，請再試一次。");
      return false;
    } finally {
      setBusy(false);
    }
  };

  const selectProvider = (next: ProviderId) => {
    if (busy() || next === provider()) return;
    setProvider(next);
    setDraft(null);
    setError("");
  };

  const save = async () => {
    const current = draft();
    if (!current || busy()) return;
    const problem = validationError(provider(), current);
    if (problem) { setError(problem); return; }
    const fields = { name: current.name.trim(), url: current.url.trim(), messagePrefix: current.messagePrefix };
    const saved = current.id
      ? await mutate("edit_project", { id: current.id, ...fields })
      : await mutate("create_project", fields);
    if (saved) setDraft(null);
  };

  const editor = () => (
    <div class="project-editor">
      <label>名稱<input value={draft()?.name ?? ""} disabled={busy()} onInput={(event) => setDraft({ ...draft()!, name: event.currentTarget.value })} /></label>
      <label>URL<input type="url" value={draft()?.url ?? ""} placeholder={choice().urlHint} disabled={busy()} onInput={(event) => setDraft({ ...draft()!, url: event.currentTarget.value })} /></label>
      <label>Message Prefix<textarea rows="3" value={draft()?.messagePrefix ?? ""} disabled={busy()} onInput={(event) => setDraft({ ...draft()!, messagePrefix: event.currentTarget.value })} /></label>
      <div class="project-actions">
        <button type="button" class="primary-button" disabled={busy()} onClick={save}>儲存</button>
        <button type="button" disabled={busy()} onClick={() => { setDraft(null); setError(""); }}>取消</button>
      </div>
      <div class="status is-error" role="status" aria-live="polite">{error()}</div>
    </div>
  );

  return (
    <>
      <h1>專案書籤</h1>
      <p class="bookmark-intro">ChatGPT（G）與 Claude（C）各自保存常用的 Project、Default Project 與 Message Prefix，互不影響。</p>

      <div class="provider-tabs" role="tablist" aria-label="書籤平台">
        <For each={PROVIDER_CHOICES}>{(item) => (
          <button
            type="button"
            role="tab"
            class="provider-tab"
            classList={{ "is-active": provider() === item.id }}
            aria-selected={provider() === item.id}
            disabled={busy()}
            onClick={() => selectProvider(item.id)}
          >
            <span class="provider-letter" aria-hidden="true">{item.letter}</span>{item.name}
          </button>
        )}</For>
      </div>

      <div class="default-project">
        <label for="default-project" class="setting-label">{choice().name} Default project</label>
        <select id="default-project" disabled={loading() || busy()} value={list().defaultProjectId ?? ""}
          onChange={async (event) => {
            const id = event.currentTarget.value || null;
            const select = event.currentTarget;
            if (!await mutate("set_default_project", { id })) {
              select.value = list().defaultProjectId ?? "";
            }
          }}>
          <option value="">{choice().builtin}</option>
          <For each={list().projects}>{(project) => <option value={project.id}>{project.name}</option>}</For>
        </select>
        <p class="setting-description">選擇 {choice().builtin} 時，新建 {choice().name} 分頁會開啟 {choice().home}。</p>
      </div>

      <div class="projects-heading">
        <h2>{choice().name} Projects</h2>
        <button type="button" class="primary-button" disabled={loading() || busy()} onClick={() => { setDraft(emptyDraft()); setError(""); }}><FiPlus aria-hidden="true" />新增 Project</button>
      </div>
      <Show when={draft()?.id === null}>{editor()}</Show>
      <Show when={!loading() && list().projects.length === 0 && draft() === null}>
        <p class="empty-projects">尚未新增 {choice().name} Project。</p>
      </Show>
      <For each={list().projects}>{(project) => (
        <Show when={draft()?.id !== project.id} fallback={editor()}>
          <article class="project-item">
            <div class="project-item-top">
              <strong>{project.name}</strong>
              <Show when={list().defaultProjectId === project.id}><span class="default-badge">Default</span></Show>
            </div>
            <div class="project-field"><span>URL</span><span class="project-value">{project.url}</span></div>
            <div class="project-field"><span>Message Prefix</span><span class="project-value prefix-value">{project.messagePrefix || "—"}</span></div>
            <div class="project-actions">
              <button type="button" disabled={busy()} onClick={() => { setDraft({ ...project }); setError(""); }}>編輯</button>
              <button type="button" class="delete-button" disabled={busy()} onClick={() => void mutate("delete_project", { id: project.id })}>刪除</button>
            </div>
          </article>
        </Show>
      )}</For>
      <Show when={draft() === null}><div class="status is-error" role="status" aria-live="polite">{error()}</div></Show>
    </>
  );
}
