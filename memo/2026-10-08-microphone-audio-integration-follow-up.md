# GPT Wrapper｜BLE 控制完成後的麥克風整合：後續討論備忘錄

- **日期**：2026-10-08
- **專案**：`gpt-wrapper`／Android PTT Controller／未來藍牙實體裝置
- **文件類型**：下一階段討論備忘錄（**不是立即執行的實作規格，也不代表已選定麥克風傳輸技術**）
- **先決階段**：Android 與 Windows 透過真正的 BLE GATT 傳送／接收 `PTT_DOWN`、`PTT_UP`、`SUBMIT`；這一版先使用 **Windows 現有麥克風** 進行 ChatGPT 聽寫。
- **本文件目的**：避免 BLE 按鍵整合完成後忘記下一階段真正要驗證的「麥克風 → Windows → ChatGPT WebView」整條音訊路徑。

## 一、目前版本與下一階段的邊界

### 目前正在規劃／交由 Agent 實作的版本

```text
Android App（未來實體硬體的控制器替身）
  └─ BLE GATT：PTT_DOWN / PTT_UP / SUBMIT
      └─ Windows GPT Wrapper
          ├─ 操作 ChatGPT WebView 的聽寫開始／結束
          └─ 操作 ChatGPT WebView 的 Send（獨立 Submit）
Windows 既有麥克風 ──────────────────────→ ChatGPT WebView 收音與轉錄
```

- Android 只負責按鍵控制，**不負責轉錄、不顯示文字，也不判斷 ChatGPT 是否完成送出**。
- Windows GPT Wrapper 負責固定目標分頁、啟停聽寫、等待轉錄完成，以及安全地執行 Submit。
- 此版本刻意**不包含** Android 麥克風音訊橋接、標準藍牙音訊、虛擬麥克風或 Audio Gate。
- 本備忘錄討論的是**上述版本完成、真實 BLE 控制已驗收之後**的工作；不應把音訊整合混進當前 BLE 控制的驗收條件。

### 最終希望達成的裝置體驗

一台可攜式裝置內有**麥克風、PTT 與 Submit 兩顆實體按鍵**。連上 Windows 後，控制與音訊長時間保持可用，無須每次 PTT 重新配對；待機時不把有效聲音送到 ChatGPT 的聽寫路徑；按住 PTT 才啟動 Windows ChatGPT 聽寫並放行聲音；放開 PTT 結束聽寫並恢復軟體靜音；按下 Submit 才請 Windows 傳送訊息。

關鍵：**聽寫與文字轉錄始終在 Windows 端的 ChatGPT 網頁中進行**。Android／實體裝置只負責按鍵與音訊來源，不提供文字轉錄功能。

## 二、下一階段需要拆開驗證的三個問題

1. **音訊傳輸（Transport）**：裝置上的麥克風聲音究竟如何穩定抵達 Windows？Windows 是否能辨識為可用的音訊輸入？Bluetooth 已配對不等於音訊 Session 永遠保持啟動。
2. **音訊選擇（Selection）**：ChatGPT WebView2 是否**確實使用使用者選到的麥克風**，而不只是 UI 裡選到該裝置？這是已確認的核心需求，詳見既有《WebView2 麥克風選擇方案討論紀錄》。
3. **PTT 音訊時序（Gate / Readiness）**：按下 PTT 後，何時才是真正「ChatGPT 已開始錄音」？放開時如何避免尾音消失？待機時如何確保不把有效聲音送進聽寫路徑？**不能把 BLE 事件已送達、Audio Gate 已開啟與 ChatGPT 已就緒當成同一件事。**

上述三件事可以分階段測試，先不綁死為單一技術方案。

## 三、音訊來源與傳輸路線：候選方案（尚未定案）

| 路線 | 最適合驗證的問題 | 限制／備註 |
| --- | --- | --- |
| **A. 現成藍牙耳麥或麥克風（Windows HFP／裝置支援時評估 LE Audio）** | 最接近未來藍牙硬體，先測 Windows 音訊裝置辨識、閒置喚醒、延遲和 WebView2 收音 | 第一階段可能是「Android BLE 雙鍵控制器＋另一支藍牙耳麥」兩個物理裝置；HFP 的啟動、音質與 Windows 相容性需實測 |
| **B. Android 麥克風 → Wi-Fi／區域網路 Audio Bridge → Windows 音訊裝置／虛擬輸入** | 用既有 Android 手機快速驗證收音體驗、時序與 Windows Audio Gate，可能降低硬體採購成本 | 能驗證音訊流程，但**不能證明未來藍牙 HFP／LE Audio 韌體與 Windows 相容**；工具（如 AudioRelay）是否符合需求要實測 |
| **C. 自訂 BLE 音訊串流 → Windows 解碼／虛擬麥克風** | 若未來硬體無法作為標準 Bluetooth 麥克風，可研究客製化傳輸 | 通訊頻寬、持續串流、延遲、功耗與驅動複雜度高；**不應假設 BLE GATT 雙按鍵協定能直接承載可用的麥克風音訊** |
| **D. 單一支援合適 Bluetooth 音訊 Profile 的整合硬體** | 最終「雙鍵＋麥克風一體化」產品 | 須先確認晶片／開發板的 HFP 或 LE Audio 實作能力、Windows 支援與成本，不能因板上有 PDM 麥克風＋BLE 就視為可用 |

**建議的討論順序**：先用 **A** 驗證 Windows 真正接收標準藍牙麥克風的風險；若需要先快速研究 Audio Gate 與時序，**B** 可作平行、較低門檻的軟體驗證。等 A/B 的實測結果明確，再決定是否有必要走 C，以及 D 應選哪一種晶片／音訊 Profile。

這是研究優先順序，不是購買或技術選型的最終決策。

## 四、軟體靜音（Software Mute）是這一階段的核心設計

已選擇的產品行為是：**連線維持常駐、麥克風音訊路徑保持就緒、PTT 只決定是否讓有效語音進入 ChatGPT**，而不是每次放開 PTT 就切掉麥克風供電或重新配對。

候選實現位置：

- **裝置端 Gate（最終硬體可研究）**：按鍵直接使裝置輸出靜音資料／解除靜音；前提是硬體／音訊 Profile 支援，而且 Windows 的錄音 Session 確實維持可用。
- **Windows 端 Audio Gate（軟體／兩件式 Prototype 可優先驗證）**：音訊來源保持連線，Windows 把聲音送進受控 Gate，再由 ChatGPT 可使用的音訊輸入路徑取得。必要時才評估虛擬麥克風／虛擬音訊驅動或現成橋接工具。
- **只切換 ChatGPT 聽寫、不設額外 Gate（最低成本基準）**：先測目前 PTT 開始／結束聽寫是否已足以滿足待機不轉錄、首尾音不遺失的體驗；若不夠再引入更複雜的 Gate。

應避免用**整個 Windows 系統的全域麥克風靜音**取代應用專用的 Gate，否則可能影響其他通話與錄音程式。

> 待機「軟體靜音」是**沒有有效音訊進入 ChatGPT 聽寫路徑**；不代表感測器一定完全停止收音。若要硬體層級隱私保障，需另立需求。

## 五、WebView2 的麥克風選擇：沿用前次結論，不預設 UI 入口

既有 memo：`memo/2026-10-08-webview2-microphone-selection-meeting-notes.md`。

已確認的需求只有一個：**User 能選到適合的 Windows 麥克風，且 ChatGPT 實際使用那一支**。Settings、Tab Bar、Windows 設定或 WebView2 內部哪個位置負責，先不強制指定。

之前保留的技術候選是：對受信任的 ChatGPT WebView 在適當時機攔截 `navigator.mediaDevices.getUserMedia()`，在保留原有 audio constraints 的前提下指定所選的 `deviceId`，再交回原始 MediaStream；也允許先驗證是否存在更低侵入、相容性更好的 Windows／WebView2 方法。

驗證必須同時包含：
- 至少兩支可區分的輸入裝置，其中一支**不是 Windows 系統預設**；
- 不只確認下拉選單及 `getSettings().deviceId`，還要用真實收音確認來源；
- ChatGPT 聽寫、多分頁、背景 Standby Tab、WebView 載入、重啟與麥克風斷線重連；
- 裝置不可用時不可悄悄改用錯誤來源。

此項可以獨立於藍牙音訊 Profile 研究，不應等最後硬體選型才開始做。

## 六、建議下一輪研究／驗證順序

### Step 1｜建立可比較的收音基準

使用現有 Windows 麥克風、已完成的 BLE PTT Controller，測量按下／放開至 ChatGPT 真正收音／停止的時間、第一字和尾字是否被吃掉。記下 Windows 裝置、WebView2 狀態、視窗前景／系統匣模式及操作時間戳。

### Step 2｜讓 Windows 真正使用另一支麥克風

優先拿現成 USB／藍牙耳麥作對照，驗證 Windows 可辨識與 ChatGPT 真正收到該來源；再測藍牙耳麥待機 30 分鐘後的首次錄音，觀察 HFP／LE Audio 可用性與是否有額外等待。此時 BLE 控制仍由 Android 負責，不需立即改成一體式硬體。

### Step 3｜比較「無 Gate」與「軟體 Gate」的實測差異

先確定無 Gate 時問題在哪裡，再評估 Windows Audio Gate／裝置端 Gate。分別測：
- PTT DOWN 到 Gate 放行的延遲；
- PTT DOWN 到 ChatGPT **真正錄到第一個音節**的端到端延遲；
- PTT UP 時尾音緩衝／延後靜音；
- 待機時不進入聽寫，且不影響其他程式的麥克風使用；
- 快速連續兩段 PTT、多段文字後獨立 Submit、斷線防卡住。

### Step 4｜根據測試結果選擇硬體路線

若現成標準藍牙麥克風可滿足需求，再研究如何將 BLE 雙鍵與符合音訊 Profile 的硬體整合到同一台裝置；若標準路線有不可接受的限制，再評估自訂音訊協定與虛擬麥克風成本。

## 七、預計驗收觀察項目（討論用，尚非承諾數字）

| 項目 | 期待觀察的結果 |
| --- | --- |
| 音訊來源 | ChatGPT 確實使用指定的麥克風，而不是誤用 Windows 預設 |
| 連線／待機 | 閒置約 30 分鐘後，仍能透過單次 PTT 開始使用；記錄是否重新啟動 Audio Session |
| 首字／尾字 | 正常說話的第一個和最後一個音節不明顯截斷 |
| 端到端延遲 | 分開量測 BLE 事件、音訊 Gate、ChatGPT 真正開始錄音；原先約 200ms 僅是 Gate 放行的探索目標，不代表 ChatGPT 啟動時間 |
| 軟體靜音 | 未按 PTT 時 ChatGPT 不接收有效語音，不意外全域靜音其他 App |
| 多分頁與後台 | PTT 鎖定目標分頁；系統匣待命分頁與 Prefix 不受影響 |
| 斷線恢復 | 麥克風或 BLE 中斷後安全回復；不能永久卡在錄音／Gate 開啟狀態 |
| 獨立 Submit | Android 只送控制事件，Windows 等待真正轉錄完成後才處理送出 |

## 八、下一次討論要決定的問題

1. **下一輪先用哪個聲音來源？** 優先借／買現成 Windows 藍牙麥克風驗證標準音訊路徑，還是先把 Android 手機音訊經 Wi-Fi Bridge 送入 Windows 來研究時序？也可以並行，但驗證目標不同。
2. **是否最終一定要一體式裝置？** 單一外殼的硬性要求會大幅影響晶片、供電與 Profile 選型。
3. **軟體靜音偏好放在哪一端？** 裝置端、Windows Audio Gate，或先測「僅 ChatGPT 聽寫啟停」是否足夠。必要時再決定虛擬麥克風。
4. **什麼時候必須開始可以說話？** 是否需要 PTT 按下前保持音訊擷取 Session 預熱、按下後的 ready 回報、頭尾緩衝？具體延遲目標以實測結果再決定。
5. **麥克風來源選擇的產品體驗**：先驗證指定的輸入真的生效，再定 Settings／Tab Bar／原生控制入口；不可只做假選擇器。
6. **測試環境與裝置條件**：Windows 版本、藍牙 adapter、Android 型號、現成耳麥和預算；先列清楚才能解讀相容性測試結果。

## 九、本 memo 的使用方式

- 當前 BLE 控制實作完成後，**從本文件的「Step 1／Step 2」重新開啟討論**，而不是直接跳到採購開發板或寫虛擬音訊驅動。
- 先回顧第一版 Android↔Windows 真實控制測試結果，再決定音訊路線、硬體選型與後續實作階段。
- 若確定技術選型，屆時另撰寫實作 md；**這份 memo 不構成立即修改 Android 或 Windows 音訊程式碼的授權**。

### 關聯文件

- `memo/2026-10-08-bluetooth-ptt-microphone-meeting-notes.md`：最初雙鍵＋藍牙麥克風產品願景。
- `memo/2026-10-08-webview2-microphone-selection-meeting-notes.md`：使用者選擇 Windows 麥克風與 WebView2 收音來源的研究結論。
- `android/.agent_temp/phase-0-shared-ble-contract.md`：**本階段** Android↔Windows BLE 控制契約（**不含音訊**）。
- `.agent_temp/ble-ptt-integration/README.md`：本階段控制整合執行順序。

---

**核心提醒：目前 BLE 雙鍵版本解決的是「控制」，下一階段真正要解決的是「麥克風音訊能長時間待命、被 ChatGPT 正確選中，並與 PTT 取得一致的啟停時序」。先驗證聲音來源與延遲，再決定 Audio Gate 和一體式藍牙硬體方案。**
