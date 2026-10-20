import { createSignal, onCleanup, onMount, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import "./DevicesSettings.css";

type Stage = "search" | "securing" | "identify" | "wifi" | "connecting" | "discovering" | "authenticating" | "preparing" | "committing" | "waiting_wifi_only" | "done" | "canceling" | "canceled" | "error" | "requires_reset";
type Target = { id: string; name: string };
type Network = { ssidHex: string; name: string; channel: number; auth: number; supported: boolean };
type Device = { id: string; name: string; ssid: string; pair: string; selected: boolean; link: string; hostAuth: boolean; error?: string };
type DeviceError = { code: string; stage: string; reason: string; retryAction: string; needsPhysicalReset: boolean };
type Wizard = { id: string; revision: number; targetId?: string; stage: Stage; targets: Target[]; networks: Network[]; error?: DeviceError; needsPhysicalReset: boolean };
type Snapshot = { revision: number; receiver: string; audio: string; selectedId?: string; devices: Device[]; wizard?: Wizard; error?: string };
type Confirm = { title: string; description: string; confirmLabel: string; danger?: boolean; action: () => void };

const stepNames = ["BLE 搜尋", "實體確認", "Wi-Fi 設定", "Host 驗證", "配對完成"];
const stages: Record<Stage, string> = {
  search: "搜尋附近可配對的 ESP32", securing: "建立安全 BLE 連線", identify: "確認正確的 ESP32 裝置",
  wifi: "設定 ESP32 的 2.4 GHz Wi-Fi", connecting: "連接 Wi-Fi", discovering: "尋找 Windows Host",
  authenticating: "驗證 Windows Host", preparing: "儲存配對資料", committing: "確認雙端 COMMIT",
  waiting_wifi_only: "確認 BLE 已關閉", done: "ESP32 配對完成", canceling: "安全取消配對中",
  canceled: "已取消", error: "配對未完成", requires_reset: "需要實體重置裝置",
};
function errorText(reason?: string | DeviceError) {
  if (!reason) return "";
  if (typeof reason === "string") return reason;
  if (reason.code === "E09") return "E09：找不到 Windows Host，可重新搜尋。";
  if (reason.code === "E08") return "E08：Wi-Fi 連線失敗（" + reason.stage + " / " + reason.reason + "），請確認網路及密碼。";
  if (reason.code === "E12") return "E12：Host 驗證失敗（" + reason.stage + " / " + reason.reason + "）。";
  if (reason.code === "E07") return "E07：實體確認逾時，可重新確認。";
  return reason.code + " / " + reason.stage + " / " + reason.reason;
}
function wizardStep(stage: Stage): number {
  if (stage === "search" || stage === "securing") return 0;
  if (stage === "identify") return 1;
  if (stage === "wifi") return 2;
  if (stage === "done") return 4;
  return 3;
}
function progressState(stage: Stage, index: number, error?: DeviceError): "done" | "running" | "failed" | "pending" {
  const mapping: Partial<Record<Stage, number>> = {
    connecting: 0, discovering: 1, authenticating: 2, preparing: 3, committing: 3, waiting_wifi_only: 3, done: 4,
  };
  const current = mapping[stage];
  if (current !== undefined) return current > index ? "done" : current === index ? "running" : "pending";
  if (stage === "error" || stage === "requires_reset") {
    const hint = (error?.stage ?? "").toLowerCase();
    const failed = hint.includes("wifi") || hint.includes("connect") ? 0 :
      hint.includes("discov") || hint.includes("host_search") ? 1 :
      hint.includes("auth") || hint.includes("verify") ? 2 : 3;
    return failed > index ? "done" : failed === index ? "failed" : "pending";
  }
  return "pending";
}

export default function DevicesSettings() {
  const [snapshot, setSnapshot] = createSignal<Snapshot>();
  const [busy, setBusy] = createSignal(false);
  const [opening, setOpening] = createSignal(false);
  const [message, setMessage] = createSignal("");
  const [chosenTarget, setChosenTarget] = createSignal("");
  const [network, setNetwork] = createSignal("");
  const [ssid, setSsid] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [consent, setConsent] = createSignal(false);
  const [readConsent, setReadConsent] = createSignal(false);
  const [confirm, setConfirm] = createSignal<Confirm | null>(null);
  let mounted = true, polling = false;
  const wizard = () => snapshot()?.wizard;
  const isAudioReady = () => snapshot()?.audio === "Ready";
  const isReceiverReady = () => snapshot()?.receiver === "Ready";
  const isOpen = () => !!wizard() && wizard()?.stage !== "canceled";
  const refresh = async () => {
    if (polling || busy()) return;
    polling = true;
    try { const next = await invoke<Snapshot>("devices_snapshot"); if (mounted) setSnapshot(next); }
    catch (error) { if (mounted) setMessage(String(error)); }
    finally { polling = false; }
  };
  onMount(() => {
    void refresh().then(() => act("recheck_audio"));
    const timer = setInterval(() => void refresh(), 500);
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (confirm()) { setConfirm(null); event.stopPropagation(); }
      else if (isOpen() && !busy()) { void dismissWizard(); event.stopPropagation(); }
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => { mounted = false; clearInterval(timer); document.removeEventListener("keydown", onKey); setPassword(""); });
  });
  const act = async (op: string, args: Record<string, unknown> = {}): Promise<Snapshot | undefined> => {
    if (busy() || !snapshot()) return;
    setBusy(true); setMessage("");
    try {
      const next = await invoke<Snapshot>("devices_action", {
        action: { op, args, wizardId: wizard()?.id ?? null, expectedRevision: snapshot()!.revision },
      });
      if (mounted) setSnapshot(next);
      if (["cancel", "retry", "add", "later", "reset_done"].includes(op)) {
        setPassword(""); setConsent(false); setReadConsent(false); setNetwork(""); setSsid(""); setChosenTarget("");
      }
      return next;
    } catch (error) {
      if (mounted) setMessage(errorText(String(error)));
    } finally { setBusy(false); void refresh(); }
  };
  const addDevice = async () => {
    setChosenTarget(""); setOpening(true);
    try { await act("add"); }
    finally { setOpening(false); }
  };
  const dismissWizard = async () => {
    if (busy()) return;
    if (wizard()?.stage === "done") await act("later");
    else await act("cancel");
  };
  const activate = async (id: string) => {
    const next = await act("select", { id });
    if (next?.wizard?.stage === "done") await act("later");
  };
  const configure = () => {
    const selected = wizard()?.networks.find(n => n.ssidHex === network());
    const args = { ...(selected ? { ssidHex: selected.ssidHex } : { ssid: ssid() }), password: password(), consent: consent() };
    setPassword("");
    void act("configure_wifi", args);
  };
  const windowsWifi = async (readPassword: boolean) => {
    if (!wizard() || busy() || !snapshot()) return;
    const selected = wizard()?.networks.find(n => n.ssidHex === network());
    const ssidHex = selected?.ssidHex ?? Array.from(new TextEncoder().encode(ssid()), b => b.toString(16).padStart(2, "0")).join("");
    setBusy(true); setMessage("");
    try {
      const result = await invoke<{ password?: string; name?: string } | null>("devices_wifi", {
        wizardId: wizard()!.id, expectedRevision: snapshot()!.revision,
        consent: readPassword && readConsent(), ssidHex: readPassword ? ssidHex : null,
      });
      if (readPassword) setPassword(result?.password ?? "");
      else if (result?.name) {
        setNetwork(""); setSsid(result.name);
        setMessage("已填入 Windows 目前網路，請確認 ESP32 可使用對應的 2.4 GHz 網路。");
      } else setMessage("沒有可推薦的 Windows Wi-Fi，請選擇 ESP32 掃描結果或手動輸入。");
    } catch { setMessage(readPassword ? "無法讀取已存密碼，請手動輸入。" : "無法讀取目前網路，請手動選擇。"); }
    finally { setBusy(false); }
  };
  const wifiValid = () => consent() && (network() ? true : !!ssid().trim() && new TextEncoder().encode(ssid()).length <= 32);
  const retryText = () => wizard()?.error?.code === "E09" ? "重新搜尋" :
    wizard()?.error?.code === "E07" ? "重新確認" :
    wizard()?.error?.code === "E08" ? "重新填寫 Wi-Fi" : "建立新連線重試";
  const requestConfirm = (data: Confirm) => setConfirm(() => data);
  const deviceResetHint = "不能從 Settings 直接覆寫已配對裝置的 Wi-Fi。請在 ESP32 上同時按住 PTT 與 Submit 5 秒，清除裝置端 Wi-Fi 與 Host 設定，進入 BLE 配網後再使用 Add Device。";

  return <div class="devices-page">
    <header class="devices-header">
      <div>
        <div class="devices-eyebrow">DEVICE MANAGEMENT</div>
        <h1>Devices</h1>
        <p class="devices-sub">管理已配對的 ESP32 無線 PTT 裝置與音訊橋接狀態。</p>
      </div>
      <button class="devices-btn primary" disabled={busy() || !snapshot() || (isOpen() && wizard()?.stage !== "done")} onClick={addDevice}>＋ Add Device</button>
    </header>

    <div class="devices-info"><strong>同一時間只有一台 ESP32 可以啟用。</strong> 選取裝置不代表立即開始收音；必須通過 Host 身分驗證與 Audio Ready。斷線不會自動切換到其他 ESP32。</div>
    <Show when={message()}><div class="devices-callout error" role="alert">{message()}</div></Show>
    <Show when={snapshot()?.error}><div class="devices-callout error" role="alert">{snapshot()?.error}</div></Show>
    <Show when={!isReceiverReady()}><div class="devices-callout warning">Receiver：{snapshot()?.receiver ?? "檢查中"}。<button class="devices-btn sm" disabled={busy()} onClick={() => void act("retry_receiver")}>重試 Receiver</button></div></Show>

    <div class="devices-section-title"><h2>ESP32 Wireless PTT</h2><small>{snapshot()?.devices.length ?? 0} 台裝置</small></div>
    <Show when={snapshot()?.devices.length} fallback={<Show when={snapshot()} fallback={<div class="devices-card devices-empty">正在載入裝置…</div>}>
      <div class="devices-card devices-empty">
        <div class="devices-empty-icon">⌁</div>
        <h3>尚未配對 ESP32</h3>
        <p class="devices-sub">新增裝置後，使用安全 BLE 完成 Wi-Fi 配網與 Host 驗證。</p>
        <button class="devices-btn primary" disabled={busy() || !snapshot()} onClick={addDevice}>＋ Add Device</button>
      </div>
    </Show>}>
      <For each={snapshot()?.devices}>{device => {
        const connected = () => device.link === "Online" && device.hostAuth;
        const usable = () => device.selected && device.pair === "Paired" && connected() && isAudioReady();
        return <article class="devices-card devices-device" classList={{ selected: device.selected }}>
          <div class="devices-device-top">
            <div class="devices-device-icon" aria-hidden="true">⌁</div>
            <div class="devices-device-name">
              <div class="devices-device-title">{device.name} <Show when={device.selected}><span class="devices-badge ok">啟用中</span></Show></div>
              <div class="devices-device-meta">ESP32 無線 PTT · ID #{device.id.slice(-6)}</div>
            </div>
            <div class="devices-device-actions">
              <Show when={!device.selected}><button class="devices-btn sm soft" disabled={busy() || device.pair !== "Paired"} onClick={() => requestConfirm({
                title: "啟用 " + device.name,
                description: "切換啟用裝置時會先安全停止現有 ESP32 聽寫並保留已輸入草稿，不會自動送出。若新裝置尚未 Ready，仍可選定，但不能開始收音。",
                confirmLabel: "設為啟用", action: () => void activate(device.id),
              })}>設為啟用</button></Show>
              <Show when={device.selected}><button class="devices-btn sm" disabled={busy()} onClick={() => void act("disable")}>停用</button></Show>
              <button class="devices-btn sm" disabled={busy()} onClick={() => requestConfirm({
                title: "更換 Wi-Fi", description: deviceResetHint, confirmLabel: "知道了", action: () => {},
              })}>更換 Wi-Fi</button>
              <button class="devices-btn sm" disabled={busy()} onClick={() => {
                const name = window.prompt("裝置名稱", device.name)?.trim();
                if (name && name !== device.name) void act("rename", { id: device.id, name });
              }}>改名</button>
              <button class="devices-btn sm danger" disabled={busy()} onClick={() => requestConfirm({
                title: "Forget " + device.name + "？",
                description: "這只會刪除此 Windows 使用者的本機配對紀錄，不會清除 ESP32 本體的 Host 綁定。重新配對前，請在 ESP32 上同按 PTT 與 Submit 5 秒重置。",
                confirmLabel: "Forget", danger: true, action: () => void act("forget", { id: device.id, confirmed: true }),
              })}>Forget</button>
            </div>
          </div>
          <div class="devices-details">
            <span>Wi-Fi <b>{device.ssid || "未設定"}</b></span>
            <span>連線 <span class="devices-badge" classList={{ ok: connected(), off: !connected() }}>● {connected() ? "已驗證" : "離線／待驗證"}</span></span>
            <span>音訊 <span class="devices-badge" classList={{ ok: connected() && isAudioReady(), warn: connected() && !isAudioReady(), off: !connected() }}>● {!connected() ? "Unavailable" : isAudioReady() ? "Audio Ready" : "Audio Error"}</span></span>
            <span>PTT <b>{usable() ? "可使用" : "已停用"}</b></span>
            <Show when={device.pair !== "Paired"}><span class="devices-badge warn">配對未完成／已隔離</span></Show>
          </div>
          <Show when={device.error}><div class="devices-callout error" role="alert">{device.error}</div></Show>
        </article>;
      }}</For>
    </Show>
    <p class="devices-section-foot">更換 Wi-Fi 需要在 ESP32 上同時長按 PTT＋Submit 5 秒，重新啟動 BLE 配網。</p>

    <div class="devices-section-title"><h2>Audio Bridge</h2><small>VB-CABLE · 由使用者自行安裝</small></div>
    <section class="devices-card devices-bridge">
      <div class="devices-bridge-head">
        <div><h3>VB-CABLE / CABLE Output</h3><p class="devices-sub">ESP32 PTT 聽寫使用此音源；一般麥克風與 Voice Mode 保留原有設定。</p></div>
        <span class="devices-badge" classList={{ ok: isAudioReady(), warn: !isAudioReady() }}>● {isAudioReady() ? "Audio Ready" : "Audio Error"}</span>
      </div>
      <div class="devices-bridge-content">
        <div><b>{isAudioReady() ? "虛擬麥克風已安裝並可使用" : "音訊橋接未就緒：" + (snapshot()?.audio ?? "檢查中")}</b><p class="devices-sub">固定 +12 dB Gain＋Limiter · Jitter Buffer 自動管理</p></div>
        <div class="devices-inline-actions">
          <button class="devices-btn" disabled={busy()} onClick={() => void act("recheck_audio")}>重新檢查</button>
          <a class="devices-btn" href="https://vb-audio.com/Cable/" target="_blank" rel="noopener noreferrer">官方下載頁 ↗</a>
        </div>
      </div>
      <Show when={!isAudioReady()}><p class="devices-section-foot">Audio 未就緒只停用 ESP32 聽寫；已驗證裝置的獨立 Submit 仍可使用。</p></Show>
    </section>
    <p class="devices-note">BLE Security-1 使用此原型專案的共用 PoP；此設定不提供量產級防冒名保證。若韌體或配對資料故障，請使用 USB / Arduino 修復相容韌體（保留 NVS，勿執行 Erase All Flash），必要時再雙鍵重置重新配對。解除安裝與忘記裝置不會清除 ESP32 端設定或 VB-CABLE。</p>

    <Show when={opening() && !isOpen()}><div class="devices-overlay" role="status"><div class="devices-modal devices-loading-modal">正在搜尋附近的 ESP32，請確認裝置正在 BLE 配網模式…</div></div></Show>
    <Show when={isOpen()}>
      <div class="devices-overlay" role="presentation">
        <section class="devices-modal" role="dialog" aria-modal="true" aria-labelledby="devices-wizard-title" aria-busy={busy()}>
          <header class="devices-modal-head">
            <div><div class="devices-eyebrow">ADD ESP32 DEVICE</div><h2 id="devices-wizard-title">{wizard()?.stage === "done" ? "ESP32 配對完成" : "新增無線 PTT 裝置"}</h2><p class="devices-sub">步驟 {wizardStep(wizard()!.stage) + 1} / 5 · {stepNames[wizardStep(wizard()!.stage)]}</p></div>
            <button class="devices-btn sm" aria-label="關閉配對流程" disabled={busy() || wizard()?.stage === "canceling"} onClick={() => void dismissWizard()}>✕</button>
          </header>
          <div class="devices-steps"><For each={stepNames}>{(label, index) => <div class="devices-step" classList={{ current: index() === wizardStep(wizard()!.stage), done: index() < wizardStep(wizard()!.stage) }} aria-current={index() === wizardStep(wizard()!.stage) ? "step" : undefined}><i>{index() < wizardStep(wizard()!.stage) ? "✓" : index() + 1}</i><span>{label}</span></div>}</For></div>
          <div class="devices-modal-body">
            <h3>{stages[wizard()!.stage]}</h3>
            <Show when={wizard()?.targetId}><p class="devices-sub">選定裝置：#{wizard()?.targetId?.slice(-6)}</p></Show>
            <Show when={message()}><div class="devices-callout error" role="alert">{message()}</div></Show>
            <Show when={wizard()?.error}><div class="devices-callout error" role="alert">{errorText(wizard()?.error)}</div></Show>
            <Show when={wizard()?.stage === "search"}>
              <p class="devices-sub">僅列出目前處於 BLE 配網廣播狀態的裝置。請確認 ESP32 已通電。</p>
              <div class="devices-options"><For each={wizard()?.targets}>{target => <label class="devices-option" classList={{ chosen: chosenTarget() === target.id }}><input type="radio" name="devices-target" checked={chosenTarget() === target.id} onChange={() => setChosenTarget(target.id)} /><div><strong>{target.name}</strong><div class="devices-option-sub">唯一識別尾碼 #{target.id.slice(-6)} · BLE 可搜尋</div></div></label>}</For></div>
              <Show when={!wizard()?.targets.length}><div class="devices-callout">尚未搜尋到裝置，請確認 Windows Bluetooth 已開啟，並讓 ESP32 進入 BLE 配網模式。</div></Show>
              <button class="devices-btn" disabled={busy()} onClick={() => void act("scan")}>↻ 重新搜尋</button>
            </Show>
            <Show when={wizard()?.stage === "securing"}><div class="devices-holdbox"><div class="devices-hold-symbol">◎</div><p>正在建立加密 BLE 連線，請保持裝置通電。</p></div></Show>
            <Show when={wizard()?.stage === "identify"}>
              <p class="devices-sub">確認選定 ESP32 的 LED 正在閃爍。先放開兩鍵，再於 30 秒內在實體裝置短按並放開 PTT。</p>
              <div class="devices-holdbox"><div class="devices-hold-symbol">◎</div><h3>等待實體確認</h3><p class="devices-sub">這次 PTT 不會開始 ChatGPT 聽寫或收音；確認結果會自動更新。</p><button class="devices-btn" disabled={busy()} onClick={() => void act("identify")}>重新讓 LED 閃爍</button></div>
            </Show>
            <Show when={wizard()?.stage === "wifi"}>
              <p class="devices-sub">Windows 可連接 5 GHz 或 Ethernet；請為 ESP32 選擇能與 Windows 互通的 2.4 GHz 網路。</p>
              <div class="devices-inline-actions devices-wifi-tools">
                <button class="devices-btn" disabled={busy()} onClick={() => void act("scan_wifi")}>↻ 掃描 2.4 GHz 網路</button>
                <button class="devices-btn" disabled={busy()} onClick={() => void windowsWifi(false)}>使用 Windows 目前網路作為候選</button>
              </div>
              <div class="devices-field"><label for="devices-wifi">Wi-Fi 網路</label><select id="devices-wifi" value={network()} onChange={e => { setNetwork(e.currentTarget.value); setPassword(""); }}><option value="">手動輸入／隱藏 SSID</option><For each={wizard()?.networks}>{n => <option value={n.ssidHex} disabled={!n.supported}>{n.name} · CH{n.channel}{n.supported ? "" : "（不支援的安全型態）"}</option>}</For></select><div class="devices-hint">網路清單由 ESP32 的 2.4 GHz Scan 提供。</div></div>
              <Show when={!network()}><div class="devices-field"><label for="devices-ssid">SSID</label><input id="devices-ssid" maxlength={32} autocomplete="off" value={ssid()} onInput={e => setSsid(e.currentTarget.value)} /></div></Show>
              <div class="devices-field"><label for="devices-password">Wi-Fi 密碼</label><input id="devices-password" type="password" autocomplete="off" maxlength={64} value={password()} onInput={e => setPassword(e.currentTarget.value)} /></div>
              <label class="devices-check"><input type="checkbox" checked={readConsent()} onChange={e => setReadConsent(e.currentTarget.checked)} />同意本次嘗試讀取 Windows 已儲存的 Wi-Fi 密碼</label>
              <button class="devices-btn" disabled={busy() || !readConsent() || !(network() || ssid())} onClick={() => void windowsWifi(true)}>嘗試讀取已存密碼</button>
              <label class="devices-check"><input type="checkbox" checked={consent()} onChange={e => setConsent(e.currentTarget.checked)} />同意將本次 Wi-Fi 設定透過加密 BLE 傳送給選定裝置</label>
            </Show>
            <Show when={["connecting", "discovering", "authenticating", "preparing", "committing", "waiting_wifi_only", "error", "requires_reset", "canceling"].includes(wizard()?.stage ?? "")}>
              <p class="devices-sub">Wi-Fi、Host 身分驗證與金鑰提交由實際裝置回報；雙端確認前不會宣告配對成功。</p>
              <div class="devices-progress"><For each={["Wi-Fi Connecting", "Host Discovery", "Authentication", "COMMIT"]}>{(label, index) => {
                const state = () => progressState(wizard()!.stage, index(), wizard()?.error);
                return <div class="devices-progress-item" classList={{ done: state() === "done", running: state() === "running", failed: state() === "failed" }}><span class="devices-progress-number">{state() === "done" ? "✓" : state() === "failed" ? "!" : index() + 1}</span><b>{label}</b><span class="devices-progress-state">{state() === "done" ? "完成" : state() === "running" ? "進行中" : state() === "failed" ? "錯誤" : "等待中"}</span></div>;
              }}</For></div>
              <Show when={wizard()?.stage === "error" && !wizard()?.needsPhysicalReset}><button class="devices-btn primary" disabled={busy()} onClick={() => void act(wizard()?.error?.code === "E07" ? "identify" : "retry")}>{retryText()}</button></Show>
              <Show when={wizard()?.needsPhysicalReset}><div class="devices-callout warning">請同按 ESP32 的 PTT 與 Submit 5 秒完成重置；未完成配對不會授權音訊或控制。</div><button class="devices-btn primary" disabled={busy()} onClick={() => void act("reset_done", { confirmed: true })}>已完成實體重置，重新新增</button></Show>
            </Show>
            <Show when={wizard()?.stage === "done"}>
              <div class="devices-success"><div class="devices-success-symbol">✓</div><h3>ESP32 配對完成</h3><p class="devices-sub">雙端 COMMIT 和 BLE 關閉已確認，現在可選擇是否啟用。</p></div>
              <div class="devices-callout" classList={{ warning: !isAudioReady() }}>{isAudioReady() ? "Audio Ready。選定啟用並驗證 Host 後即可使用 ESP32 PTT。" : "已配對，但 VB-CABLE 尚未就緒；可以稍後啟用，修復音訊前不會收音。"}</div>
            </Show>
          </div>
          <footer class="devices-modal-footer">
            <div><Show when={wizard()?.stage === "identify" || wizard()?.stage === "wifi"}><button class="devices-btn" disabled={busy()} onClick={() => void act("back")}>返回</button></Show></div>
            <div class="devices-inline-actions">
              <Show when={wizard()?.stage !== "done"}><button class="devices-btn" disabled={busy() || wizard()?.stage === "canceling"} onClick={() => void dismissWizard()}>取消</button></Show>
              <Show when={wizard()?.stage === "search"}><button class="devices-btn primary" disabled={busy() || !wizard()?.targets.some(t => t.id === chosenTarget())} onClick={() => void act("choose", { id: chosenTarget() })}>下一步 →</button></Show>
              <Show when={wizard()?.stage === "wifi"}><button class="devices-btn primary" disabled={busy() || !wifiValid()} onClick={configure}>連線並配對 →</button></Show>
              <Show when={wizard()?.stage === "done"}>
                <button class="devices-btn" disabled={busy()} onClick={() => void act("later")}>稍後啟用</button>
                <button class="devices-btn primary" disabled={busy() || !wizard()?.targetId} onClick={() => void activate(wizard()!.targetId!)}>啟用此裝置</button>
              </Show>
            </div>
          </footer>
        </section>
      </div>
    </Show>
    <Show when={confirm()}><div class="devices-overlay confirm-overlay" role="presentation">
      <section class="devices-modal devices-confirm-modal" role="dialog" aria-modal="true" aria-labelledby="devices-confirm-title">
        <header class="devices-modal-head"><h2 id="devices-confirm-title">{confirm()!.title}</h2></header>
        <div class="devices-modal-body"><p>{confirm()!.description}</p></div>
        <footer class="devices-modal-footer"><button class="devices-btn" onClick={() => setConfirm(null)}>返回</button><button class="devices-btn" classList={{ danger: confirm()?.danger, primary: !confirm()?.danger }} onClick={() => { const action = confirm()?.action; setConfirm(null); action?.(); }}>{confirm()!.confirmLabel}</button></footer>
      </section>
    </div></Show>
  </div>;
}
