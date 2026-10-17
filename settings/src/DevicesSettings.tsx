import { createSignal, onCleanup, onMount, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

type Stage = "search" | "securing" | "identify" | "wifi" | "connecting" | "discovering" | "authenticating" | "preparing" | "committing" | "waiting_wifi_only" | "done" | "canceling" | "canceled" | "error" | "requires_reset";
type Target = { id: string; name: string };
type Network = { ssidHex: string; name: string; channel: number; auth: number; supported: boolean };
type Device = { id: string; name: string; ssid: string; pair: string; selected: boolean; link: string; hostAuth: boolean; error?: string };
type DeviceError = { code: string; stage: string; reason: string; retryAction: string; needsPhysicalReset: boolean };
type Wizard = { id: string; revision: number; targetId?: string; stage: Stage; targets: Target[]; networks: Network[]; error?: DeviceError; needsPhysicalReset: boolean };
type Snapshot = { revision: number; receiver: string; audio: string; selectedId?: string; devices: Device[]; wizard?: Wizard; error?: string };
const labels: Record<Stage, string> = {
  search: "搜尋並選擇裝置", securing: "建立加密 BLE 連線", identify: "確認目標裝置", wifi: "選擇 2.4 GHz Wi-Fi",
  connecting: "連接 Wi-Fi／取得 IP", discovering: "搜尋 Host", authenticating: "驗證 Host", preparing: "保存候選配對資料",
  committing: "確認雙端保存", waiting_wifi_only: "確認裝置已關閉 BLE", done: "配對成功", canceling: "正在安全取消",
  canceled: "已取消", error: "配對未完成", requires_reset: "配對結果不明，需要裝置重置",
};
function errorText(reason?: string | DeviceError) {
  if (!reason) return "";
  if (typeof reason === "string") return reason;
  if (reason.code === "E09") return "E09：找不到 Host。";
  if (reason.code === "E08") return `E08：Wi-Fi 連線失敗（${reason.stage} / ${reason.reason}），請確認網路及密碼。`;
  if (reason.code === "E12") return `E12：Host 驗證失敗（${reason.stage} / ${reason.reason}）。`;
  if (reason.code === "E07") return "E07：實體確認逾時，可重新確認。";
  return `${reason.code} / ${reason.stage} / ${reason.reason}`;
}
export default function DevicesSettings() {
  const [snapshot, setSnapshot] = createSignal<Snapshot>();
  const [busy, setBusy] = createSignal(false);
  const [message, setMessage] = createSignal("");
  const [network, setNetwork] = createSignal("");
  const [ssid, setSsid] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [consent, setConsent] = createSignal(false);
  const [readConsent, setReadConsent] = createSignal(false);
  let mounted = true, polling = false;
  const wizard = () => snapshot()?.wizard;
  const refresh = async () => {
    if (polling || busy()) return;
    polling = true;
    try { const next = await invoke<Snapshot>("devices_snapshot"); if (mounted) setSnapshot(next); }
    catch (error) { if (mounted) setMessage(String(error)); }
    finally { polling = false; }
  };
  onMount(() => { void refresh().then(() => act("recheck_audio")); const timer = setInterval(() => void refresh(), 500); onCleanup(() => { mounted = false; clearInterval(timer); setPassword(""); }); });
  const act = async (op: string, args: Record<string, unknown> = {}) => {
    if (busy() || !snapshot()) return;
    setBusy(true); setMessage("");
    try {
      const next = await invoke<Snapshot>("devices_action", { action: { op, args, wizardId: wizard()?.id ?? null, expectedRevision: snapshot()!.revision } });
      setSnapshot(next);
      if (["cancel", "retry", "add"].includes(op)) { setPassword(""); setConsent(false); setNetwork(""); }
    } catch (error) { setMessage(errorText(String(error))); }
    finally { setBusy(false); void refresh(); }
  };
  const step = () => {
    const stage = wizard()?.stage;
    if (stage === "search" || stage === "securing") return 1;
    if (stage === "identify") return 2;
    if (stage === "wifi") return 3;
    if (stage === "done") return 5;
    return 4;
  };
  const configure = () => {
    const selected = wizard()?.networks.find(n => n.ssidHex === network());
    const args = { ...(selected ? { ssidHex: selected.ssidHex } : { ssid: ssid() }), password: password(), consent: consent() };
    setPassword(""); void act("configure_wifi", args);
  };
  const windowsWifi = async (readPassword: boolean) => {
    if (!wizard() || busy()) return;
    const selected = wizard()?.networks.find(n => n.ssidHex === network());
    const ssidHex = selected?.ssidHex ?? Array.from(new TextEncoder().encode(ssid()), b => b.toString(16).padStart(2, "0")).join("");
    setBusy(true);
    try {
      const result = await invoke<{ password?: string; name?: string } | null>("devices_wifi", { wizardId: wizard()!.id, expectedRevision: snapshot()!.revision, consent: readPassword && readConsent(), ssidHex: readPassword ? ssidHex : null });
      if (readPassword) setPassword(result?.password ?? "");
      else if (result?.name) { setNetwork(""); setSsid(result.name); setMessage("已填入 Windows 目前網路，請確認 ESP32 可使用此網路的 2.4 GHz。也可改選其他網路。"); }
      else setMessage("沒有可推薦的 Windows Wi-Fi；請選擇 ESP32 掃描結果或手動輸入。");
    } catch { setMessage(readPassword ? "無法讀取已存密碼，請手動輸入。" : "無法讀取目前網路，請手動選擇。"); }
    finally { setBusy(false); }
  };
  return <>
    <h1>Devices</h1>
    <p>ESP32 Wireless PTT · Receiver：{snapshot()?.receiver ?? "載入中"}</p>
    <Show when={snapshot()?.receiver !== "Ready"}><button disabled={busy()} onClick={() => void act("retry_receiver")}>重試 Receiver</button></Show>
    <p class="setting-description">配對與音訊為不同狀態。Audio 未就緒時無法使用 ESP32 開麥；鍵盤及一般麥克風維持可用。</p>
    <p class="setting-description">個人自用、非量產：BLE 使用 Security-1 與 App 自動提供的共用固定 PoP，不具量產級防冒名能力。每次配對仍建立獨立隨機金鑰，保護 Wi-Fi 控制與音訊；不需要首次 USB Owner enrollment 或 Security-2。</p>
    <Show when={message()}><p role="alert">{message()}</p></Show>
    <Show when={snapshot()?.error}><p role="alert">{snapshot()?.error}</p></Show>
    <For each={snapshot()?.devices}>{device => <section class="device-card">
      <h2>{device.name}</h2>
      <p>{device.id.slice(-6)} · {device.pair === "Paired" ? "已配對" : "未完成／已隔離"} · {device.selected ? "已選定啟用" : "未啟用"} · {device.link === "Online" ? "在線" : "離線"} · Host：{device.hostAuth ? "已驗證" : "未驗證"}</p>
      <Show when={device.error}><p role="alert">{device.error}</p></Show>
      <p>SSID：{device.ssid} · Audio Bridge：{snapshot()?.audio} · 此裝置音訊：{device.selected && device.hostAuth ? snapshot()?.audio : "Gate 關閉（未啟用／未連線）"}</p>
      <p class="setting-description">更換 Wi-Fi／Host：先在裝置同按 PTT 與 Submit 五秒重置，再 Add Device。此處不會修改已配對裝置的 Wi-Fi。</p>
      <button disabled={busy() || device.selected || device.pair !== "Paired"} onClick={() => void act("select", { id: device.id })}>啟用此裝置</button>
      <Show when={device.selected}><button disabled={busy()} onClick={() => void act("disable")}>停用</button></Show>
      <button disabled={busy()} onClick={() => { const name = window.prompt("裝置名稱", device.name); if (name) void act("rename", { id: device.id, name }); }}>改名</button>
      <button disabled={busy()} onClick={() => { if (window.confirm("只忘記此 Windows 帳號的本機記錄。裝置仍綁定原 Host，請同按 PTT 與 Submit 五秒重置後重新配網。")) void act("forget", { id: device.id, confirmed: true }); }}>忘記裝置</button>
    </section>}</For>
    <Show when={!snapshot()?.devices.length}><p>尚無已配對裝置。</p></Show>
    <Show when={!wizard() || wizard()?.stage === "done" || wizard()?.stage === "canceled"}>
      <button disabled={busy() || !snapshot()} onClick={() => void act("add")}>Add Device</button>
    </Show>
    <Show when={wizard() && wizard()?.stage !== "canceled"}><section class="device-wizard" aria-label="新增 ESP32 裝置" aria-busy={busy()}>
      <ol class="device-steps"><For each={["BLE 搜尋", "實體確認", "Wi-Fi", "Host 與提交", "完成"]}>{(label, index) => <li aria-current={step() === index() + 1 ? "step" : undefined}>{label}</li>}</For></ol>
      <h2>{labels[wizard()!.stage]}</h2>
      <Show when={wizard()?.targetId}><p>選定裝置：{wizard()?.targetId}</p></Show>
      <Show when={wizard()?.error}><p role="alert">{errorText(wizard()?.error)}</p></Show>
      <Show when={wizard()?.stage === "search"}>
        <For each={wizard()?.targets}>{target => <button disabled={busy()} onClick={() => void act("choose", { id: target.id })}>{target.name} · {target.id.slice(-6)}</button>}</For>
        <Show when={!wizard()?.targets.length}><p>未找到裝置，請確認 Windows Bluetooth 已開啟且裝置正在配網。</p></Show>
        <button disabled={busy()} onClick={() => void act("scan")}>重新掃描</button>
      </Show>
      <Show when={wizard()?.stage === "identify"}>
        <p>確認目標 LED 正在閃爍，先放開兩鍵，再於 30 秒內短按並放開 PTT。這次按鍵不會收音。</p>
        <button disabled={busy()} onClick={() => void act("identify")}>重新確認</button>
      </Show>
      <Show when={wizard()?.stage === "wifi"}>
        <p>Windows 可連接 5 GHz 或 Ethernet；請為 ESP32 選擇可與此電腦互通的 2.4 GHz 網路，也可填入隱藏 SSID。</p>
        <button disabled={busy()} onClick={() => void act("scan_wifi")}>掃描裝置可見網路</button>
        <button disabled={busy()} onClick={() => void windowsWifi(false)}>使用 Windows 目前網路作為候選</button>
        <label>Wi-Fi<select value={network()} onChange={e => setNetwork(e.currentTarget.value)}><option value="">手動／隱藏 SSID</option><For each={wizard()?.networks}>{n => <option value={n.ssidHex} disabled={!n.supported}>{n.name} · CH{n.channel}{n.supported ? "" : "（不支援的安全型態）"}</option>}</For></select></label>
        <Show when={!network()}><label>SSID<input maxlength={32} value={ssid()} onInput={e => setSsid(e.currentTarget.value)} autocomplete="off" /></label></Show>
        <label>Wi-Fi 密碼<input type="password" value={password()} onInput={e => setPassword(e.currentTarget.value)} autocomplete="off" maxlength={64} /></label>
        <label><input type="checkbox" checked={readConsent()} onChange={e => setReadConsent(e.currentTarget.checked)} />允許本次嘗試讀取 Windows 已儲存的 Wi-Fi 密碼。</label>
        <button disabled={busy() || !readConsent() || (!network() && !ssid())} onClick={() => void windowsWifi(true)}>嘗試讀取已存密碼</button>
        <label><input type="checkbox" checked={consent()} onChange={e => setConsent(e.currentTarget.checked)} />我同意將本次 Wi-Fi 設定透過加密 BLE 傳送給選定裝置。</label>
        <button disabled={busy() || !consent() || (!network() && !ssid())} onClick={configure}>連線並配對</button>
      </Show>
      <Show when={wizard()?.stage === "done"}>
        <p>雙端提交及 BLE 關閉已確認。請明確選擇是否啟用；音訊狀態請見 Audio Bridge。</p>
        <button disabled={busy()} onClick={() => void act("select", { id: wizard()?.targetId })}>啟用此裝置</button>
        <button disabled={busy()} onClick={() => void act("later")}>稍後啟用</button>
      </Show>
      <Show when={wizard()?.stage === "error" && !wizard()?.needsPhysicalReset}>
        <button disabled={busy()} onClick={() => void act(wizard()?.error?.code === "E07" ? "identify" : "retry")}>{wizard()?.error?.code === "E09" ? "重新搜尋" : wizard()?.error?.code === "E07" ? "重新確認" : wizard()?.error?.code === "E08" ? "重新填寫 Wi-Fi" : "建立新連線重試"}</button>
      </Show>
      <Show when={wizard()?.needsPhysicalReset}>
        <p>請同按裝置 PTT 與 Submit 五秒完成重置，再重新 Add Device；未完成配對不會授權音訊或控制。</p>
        <button disabled={busy()} onClick={() => void act("reset_done", { confirmed: true })}>已完成裝置重置，重新新增</button>
      </Show>
      <Show when={["identify", "wifi"].includes(wizard()?.stage ?? "")}><button disabled={busy()} onClick={() => void act("back")}>上一步</button></Show>
      <Show when={!["done", "canceled"].includes(wizard()?.stage ?? "")}><button disabled={busy()} onClick={() => void act("cancel")}>取消</button></Show>
    </section></Show>
    <section class="device-card"><h2>Audio Bridge</h2><p>{snapshot()?.audio ?? "檢查中"}</p><button disabled={busy()} onClick={() => void act("recheck_audio")}>重新檢查</button><p>Audio 未就緒只停用 ESP32 聽寫；已驗證裝置的新獨立 Submit 仍可用。</p><p>VB-CABLE 需自行安裝或修復，完成後重新檢查。<a href="https://vb-audio.com/Cable/" target="_blank" rel="noopener noreferrer">官方下載與說明</a></p></section>
    <section class="device-card"><h2>USB 韌體修復</h2><p>裝置版本不相容、韌體故障或配對金鑰遺失時，控制與音訊會停用。使用 Arduino／USB 重刷相容韌體，保留 NVS；無法正常進入上傳模式時，按住 BOOT，短按 RESET，再放開 BOOT，選擇重新出現的 USB 埠。若設定已遺失，雙鍵五秒重置後重新配對。</p><p>不要執行 Erase All Flash。忘記裝置只清除此 Windows 帳號的本機資料，不清 Windows Bluetooth bond；ESP32 仍需實體重置。</p></section>
    <section class="device-card"><h2>更新與解除安裝</h2><p>相容 App 更新保留本帳號的配對、選定裝置與設定；更新後須重新驗證 Host 與 Audio Ready，PTT 先放開再按。正常解除安裝會清除此帳號的 ESP32 配對檔；不清 ESP32 端 Wi-Fi／Host、不移除 VB-CABLE 或 Windows Bluetooth bond。重新安裝後，請先雙鍵五秒重置 ESP32，再 Add Device。</p><p>若卸載顯示 E23，代表可能仍有配對資料：先退出 App，以同一 Windows 帳號重試卸載；依錯誤提示檢查檔案權限及殘留路徑，不能視為清理成功。</p></section>
  </>;
}
