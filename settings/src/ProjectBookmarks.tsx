import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { FiPlus } from "solid-icons/fi";

type ProjectBookmark = { id: string; name: string; url: string; messagePrefix: string };
type ProjectSettings = { defaultProjectId: string | null; projects: ProjectBookmark[] };
type Draft = { id: string | null; name: string; url: string; messagePrefix: string };

const emptyDraft = (): Draft => ({ id: null, name: "", url: "", messagePrefix: "" });

function validationError(draft: Draft): string {
  if (!draft.name.trim()) return "請輸入 Project 名稱。";
  if (!draft.url.trim()) return "請輸入 Project URL。";
  try {
    const url = new URL(draft.url.trim());
    if (url.protocol !== "http:" && url.protocol !== "https:") throw new Error("scheme");
  } catch {
    return "請輸入有效的 http:// 或 https:// URL。";
  }
  return "";
}

export default function ProjectBookmarks() {
  const [settings, setSettings] = createSignal<ProjectSettings>({ defaultProjectId: null, projects: [] });
  const [loading, setLoading] = createSignal(true);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const [draft, setDraft] = createSignal<Draft | null>(null);

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
      const latest = await invoke<ProjectSettings>(command, args);
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

  const save = async () => {
    const current = draft();
    if (!current || busy()) return;
    const problem = validationError(current);
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
      <label>URL<input type="url" value={draft()?.url ?? ""} disabled={busy()} onInput={(event) => setDraft({ ...draft()!, url: event.currentTarget.value })} /></label>
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
      <p class="bookmark-intro">儲存常用的 ChatGPT Project，並設定開啟新分頁時預設使用的 Project。</p>

      <div class="default-project">
        <label for="default-project" class="setting-label">Default project</label>
        <select id="default-project" disabled={loading() || busy()} value={settings().defaultProjectId ?? ""}
          onChange={async (event) => {
            const id = event.currentTarget.value || null;
            const select = event.currentTarget;
            if (!await mutate("set_default_project", { id })) {
              select.value = settings().defaultProjectId ?? "";
            }
          }}>
          <option value="">chatgpt.com</option>
          <For each={settings().projects}>{(project) => <option value={project.id}>{project.name}</option>}</For>
        </select>
        <p class="setting-description">選擇 chatgpt.com 時，新建 ChatGPT 分頁會開啟 https://chatgpt.com。</p>
      </div>

      <div class="projects-heading">
        <h2>Projects</h2>
        <button type="button" class="primary-button" disabled={loading() || busy()} onClick={() => { setDraft(emptyDraft()); setError(""); }}><FiPlus aria-hidden="true" />新增 Project</button>
      </div>
      <Show when={draft()?.id === null}>{editor()}</Show>
      <Show when={!loading() && settings().projects.length === 0 && draft() === null}>
        <p class="empty-projects">尚未新增 Project。</p>
      </Show>
      <For each={settings().projects}>{(project) => (
        <Show when={draft()?.id !== project.id} fallback={editor()}>
          <article class="project-item">
            <div class="project-item-top">
              <strong>{project.name}</strong>
              <Show when={settings().defaultProjectId === project.id}><span class="default-badge">Default</span></Show>
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
