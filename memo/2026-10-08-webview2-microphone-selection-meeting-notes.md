# GPT Wrapper｜WebView2 麥克風選擇方案討論紀錄

- **日期**：2026-10-08
- **專案**：`gpt-wrapper`
- **主題**：讓使用者能在 GPT Wrapper 的 ChatGPT 聽寫／語音功能中，選擇合適的 Windows 音訊輸入裝置
- **文件性質**：會議紀錄／研究結論（不是要求立即實作的規格）
- **背景關聯**：後續藍牙 PTT 麥克風 Prototype；參見 `memo/2026-10-08-bluetooth-ptt-microphone-meeting-notes.md`

## 一、最重要的產品需求（使用者已明確確認）

**使用者真正關心的只有：最終產品讓 User 能夠選到正確、合適且實際用於 ChatGPT 收音的麥克風。**

- 不強制設定入口一定要放在 GPT Wrapper 的 Settings、Tab Bar，或瀏覽器原生「網站資訊」選單。
- 不強制必須由 Tauri、WebView2、Windows、ChatGPT 網頁、或特定模組負責設定。
- 實作方可以自行選擇穩定、容易理解的方案；不要為了維持某個 UI 或技術設計而犧牲實際裝置選擇能力。
- 這項能力也要能容納未來接入的 USB／藍牙耳麥、藍牙 PTT 原型等 Windows 可辨識的輸入裝置。
- **「可以選到」不等於單純顯示裝置清單**；實際開始聽寫後，音訊必須來自所選裝置，並能確認或驗證。

## 二、目前 GPT Wrapper 的相關現況

先前檢視專案時確認：
- Windows 版採 **Tauri 2 + WebView2**，ChatGPT 網頁使用獨立 WebView 分頁。
- `src-tauri/src/main.rs` 以 `WebviewBuilder::initialization_script(ADAPTER_SCRIPT)` 注入 `src-tauri/src/chatgpt_adapter.js`，有提前介入網頁腳本的基礎。
- WebView2 與 Rust 間已有訊息傳遞機制；Settings 前端是 SolidJS，General 設定由 `src-tauri/src/general_settings.rs` 持久化。
- PTT Shortcut 控制 ChatGPT 網頁內的聽寫開始／結束，另有轉錄後自動送出選項。
- 支援多分頁與背景預先載入的 PTT Standby Tab；選用麥克風若做在 Wrapper 層，必須顧及這些情境。

上述是本次研究時的程式碼現況，不代表已實作麥克風選擇功能。

## 三、研究過的方案與結論

| 方案 | 研究結論 | 定位 |
| --- | --- | --- |
| 直接叫出 Chrome／Edge 地址列「查看網站資訊」浮動設定 | WebView2 未提供可依賴的公開 API 直接開啟這個瀏覽器 UI；Wrapper 本身也沒有完整地址列 | 不作為主要路線 |
| 從 `navigator` 開啟 Browser Audio Settings | 沒有標準 Web API 能直接開麥克風裝置設定視窗；`selectAudioOutput()` 選的是**輸出**裝置 | 不可行 |
| WebView2 Profile／網站 Permission API | 可管理麥克風**允許／拒絕**權限，但不等於指定特定輸入裝置 | 無法單獨滿足需求 |
| Chromium DevTools／啟動參數／Profile Preferences | 有內部音訊偏好機制，但不是可依賴的 WebView2 公開裝置選擇 API，版本相容與持久化未驗證 | 暫不採用 |
| Windows 音訊／每 App 音量混音程式 | 可能有系統層級選項，但 WebView2 是否能獨立指定與生效尚未實機確認 | 可作低侵入性替代方案驗證 |
| **WebView `getUserMedia()` 攔截** | 在 ChatGPT 取得音訊時加入所選 `deviceId`，保留原始 MediaStream 和原站錄音流程；不需改 Windows 全域預設裝置 | **目前保留的主要技術候選／保底方案** |

補充：Chrome／Edge 自己的瀏覽器設定與 WebView2 Host 可呼叫的 API 並非同一回事。即使一般 Chrome 能在瀏覽器 UI 選麥克風，也不能假設 Tauri WebView2 能直接呼叫相同 UI。

## 四、目前保留的 getUserMedia 技術路線

**原則：只控制輸入裝置的選擇，不自行捕捉／編碼／轉送音訊，也不重新實作 ChatGPT 聽寫。**

概念資料流：

```text
User 選擇 Windows 可用麥克風（選單形式與擺放位置可調整）
  → 保存麥克風偏好
  → 同步到 ChatGPT WebView（含多分頁與 Standby Tab）
  → ChatGPT 呼叫 navigator.mediaDevices.getUserMedia(...)
  → Wrapper 的注入腳本在適用情況下加入選定 deviceId
  → 呼叫原始 getUserMedia(...)
  → 原生 MediaStream 直接交回 ChatGPT
  → ChatGPT 原本的聽寫／停止／轉錄／送出流程不變
```

關鍵設計注意：
1. `enumerateDevices()` 能提供 `audioinput` 清單，但裝置名稱／完整列表受網站權限、頁面可見性與瀏覽器隱私策略限制；首次使用可能需先授權。
2. Web `deviceId` 是**網站來源與瀏覽器設定檔相關**的識別值，不能假設等同 Windows MMDevice 原生 ID。較合理的起點是在 **ChatGPT 網頁來源的 WebView** 列舉，再將清單提供給選單 UI。
3. 若指定裝置，`getUserMedia({audio: {deviceId: {exact: ...}}})` 可要求該裝置；應盡量保留 ChatGPT 其餘 audio constraints（如降噪、回音消除等）。
4. 若 ChatGPT 明確自行指定 `deviceId`、使用不同擷取途徑、或在子 Frame 執行收音，需要個別相容性驗證；**不能只因成功注入腳本便宣稱收音來源已改變**。
5. 改設定應在**下一次開始收音**時生效；已建立中的 MediaStream 不會因此自動替換。
6. 裝置被拔除、權限不足、`deviceId` 失效或重置時，應有清楚的錯誤／重選處理；是否回退到 Default 應明確告知，不能默默用錯裝置。
7. 攔截應限制在受信任且相關的網頁來源；避免影響非 ChatGPT 分頁或其他音訊 API。

## 五、UI 位置與責任歸屬：刻意不鎖定

曾討論的入口包含：
- **Settings → General → Microphone** 下拉選單；
- **Tab Bar 前方的齒輪 icon**，點擊打開麥克風選單；
- 若可確認可用，交由**瀏覽器／Windows 原生設定**提供裝置選擇。

**這些只是可接受的候選方案，並非使用者已指定的設計。** 後續實作 Agent 可依相容性、操作成本與維護性自行選擇最佳入口，也可提出不同方案。驗收以實際裝置選擇成功為準，不以特定 UI 擺放位置或「誰負責選擇」為準。

## 六、建議驗證順序（尚未授權執行）

1. **最小技術驗證**：在 Windows 同時接上兩個可區分的麥克風，讓 ChatGPT WebView 指向**非系統預設**的其中一支；用 `MediaStreamTrack.getSettings().deviceId` 搭配實際收音測試確認來源。
2. 驗證 **PTT 聽寫**、手動 ChatGPT 錄音／語音互動（如有使用），以及放開 PTT 後轉錄和自動送出流程沒有受影響。
3. 若成功，再把裝置列舉、選擇、儲存與同步串起來，處理多分頁、背景 Standby Tab、網頁重載。
4. 測試 USB／藍牙裝置斷線與重連、首次麥克風授權、裝置 ID 變化、Settings 開啟時 ChatGPT 頁面隱藏等例外。
5. 若有更簡單、可靠的 Windows／WebView2 原生方案，允許替代 `getUserMedia()` 攔截；但仍須通過同樣的實際收音驗證。

## 七、成功條件與尚未決定的細節

**核心成功條件**：
- User 能辨識並選到想使用的輸入裝置。
- ChatGPT 開始收音時使用該裝置，而不是無提示地使用另一支。
- 不破壞既有 PTT Shortcut、聽寫、轉錄、多分頁及背景待命流程。
- 裝置不可用時能提供可理解的回復方式。

**可以留給後續實作方決定**：
- 設定入口與視覺表現、誰負責列舉裝置、偏好如何保存／同步；
- 是否優先選用原生 Windows／WebView2 方法，而非注入攔截；
- 遇到裝置失效時的具體 UI 和回退策略（但必須避免無提示使用錯誤裝置）。

---

**本次最終共識**：保留 `getUserMedia()` 攔截作為目前最具體的技術候選，但**真正確定的是使用者需求：「讓 User 可以選擇合適的麥克風，而且 ChatGPT 確實使用它。」** 設定放在哪裡、由哪一層執行都可以由後續設計／實作選擇。此紀錄不構成立即修改程式碼的授權。
