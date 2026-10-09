# BLE CCCD Handle 對齊測試 — 結論備忘錄

日期：2026-10-09（Asia/Taipei，UTC+08:00）
狀態：**實機分組測試已執行；root cause INCONCLUSIVE；尚未取得實際 ATT Write Handle。**
對應需求：`.agent_temp/ble-handle-alignment/implementation.md`（保留）

## 1. 可直接使用的結論

原流程已重現 Windows CCCD `0x80650003`。Windows Uncached Service / Characteristic 一直回報 `0x009F / 0x00A0`。加入本次 B 組 descriptor / Cached 比較查詢後能訂閱，但這是診斷實驗，**不是修復，也不足以證明 stale ATT Handle**。

真正的 Android ATT CCCD Handle、ATT Write Request Handle、Error Response opcode / handle / code 與同連線 HCI frame mapping 全部 **N/A**。取得的 bugreport 只有裁剪 BTSNOOP summary，沒有 ACL / ATT PDU；不能用它判定 NOT_SENT。Android callback 缺席也不能代替封包證據。

## 2. 環境與保護

- Windows 11 Pro build 26200；TP-Link Bluetooth USB Adapter。
- Samsung SM-S9160；Android 16；debug APK v0.1.0；ADB platform-tools。
- Windows commit：`983dc285b0af553eb34f9082541c3b64104e0c04`。
- Android 獨立 repo commit：`ea84f902a4c7272d6b5185716c83cc85e5680d61`。
- 起始 root status：只有本 memo 尚未追蹤；Android status 空白。兩個正式原始檔修改前逐 byte 備份。
- 起始 Windows Enabled=true、已有 approved identity；正式 worker 經既有 `should_connect` 才開始 session，IsPaired=true / identity match=true。未按 Windows Scan / Approve / Forget，未改配對與核准設定。
- Android 起始 OFFLINE；以 ADB uiautomator 尋找 Start / Stop 文字與 bounds 操作，沒有輸入 PTT / SUBMIT。
- 原生 GUI skill `computer-use` 的 `orca skills get computer-use --json` 無法執行：PowerShell 回報 orca 無法辨識。改用 ADB，不使用未載入的 Orca GUI 命令。
- 正式 Tauri dev 程序偵測 Rust 插樁後自行重新編譯／重啟；沒有啟動獨立 BLE probe。程序檢查未發現舊 P5 probe。Android 每代仍觀察到兩筆 BONDED candidate connection，**來源與實際 HCI connection mapping 未驗證**，不能假設同一條 link。
- Windows 日誌存 UTC epoch ms，memo 轉 UTC ISO8601；每次 connect 有 monotonic elapsed。Android log 含 UTC ISO8601、elapsedRealtime、generation。一次 ADB 時鐘取樣顯示 Android 約落後 Windows 736 ms（取樣 RTT 78 ms）；時間只能提供關聯線索，不能證明同連線。

## 3. 輪數與分組

| 組別 | 實際 Start | Stop/Start | Windows READY | CCCD 0x80650003 | 其他失敗 |
| --- | ---: | ---: | ---: | ---: | ---: |
| A01：初始 OFFLINE 基線 | 1 | 0 | 1 | 0 | 0 |
| A：被動量測（A02–A06） | 5 | 5 | 0 | 5 | 0 |
| B：descriptor 探索／Cached 比較 | 15 | 15 | 15 | 0 | 0 |
| 合計（含初始基線） | 21 | 20 | 16 | 5 | 0 |

執行順序：A01 初始廣播 → A02–A05 四輪原流程 → B01–B15 → A06 關閉主動探索後回測。初始手機尚未廣播，A01 不是 Stop/Start，因此追加 A06 才補足五輪 A 與十五輪 B 的 **20 次實際 Stop/Start**。A06 在 B 後執行，存在先前 cache 與時序的 carryover；各組結果不能混算因果。每輪在首個完成的 connect attempt 後記錄結果；worker 自動重試仍依原 cooldown，重試次數不當成新增 Stop/Start 輪數。

實驗旗標：Windows 使用忽略的 captures/mode，A 才記錄被動資料；B 才新增 discovery。Android 使用 app 私有 `files/ble-handle-alignment-enabled` 控制量測 log。旗標預設不存在即停用。沒有更動 worker gate、backoff、protocol、UUID、permissions、ownership 決策、廣播生命周期或 CCCD write API。

所有 B 輪查詢順序相同（這是混合探索實驗，不能歸因於單一查詢）：

1. 原流程 Uncached Service → Uncached Characteristic。
2. 在此 Uncached Characteristic 上查 Cached CCCD → Uncached CCCD。
3. Cached Service → Cached Characteristic。
4. 在 Cached Characteristic 上查 Cached CCCD → Uncached CCCD。
5. 保留原 `WriteClientCharacteristicConfigurationDescriptorAsync(Indicate)`。

首個 Cached CCCD 共 15 次回報 Success 但 **zero matches**，列為該查詢的資料缺失，不能當成「Handle 相同」。Uncached 成功取得的 WinRT CCCD Handle 集合為 `0x00A2`；後續 Cached CCCD 也得到此 Handle。Cached 查詢不保證不碰遠端；B 新增查詢及耗時可能刷新 cache / 改變時序。

## 4. 每輪去識別化對照

下表 UTC 為 Windows Start 按鈕後至觀察結束；session / generation 表示相同測試區間，**不宣稱有 HCI 證實的同 link mapping**。所有實際 ATT Write 與 Android ATT UUID→Handle mapping 均缺失。A 組 Cached / CCCD 未查為 N/A，B 組 Service / Char Cached 與 Uncached 回報相同數值。Windows API 的 Characteristic.AttributeHandle 不用來猜實際寫入屬性。

| Round | 開始 → 結束 UTC | Windows session / Android generation | WinRT Service / Char / CCCD | Android ATT CCCD / ATT Write | 結果 |
| --- | --- | --- | --- | --- | --- |
| A01（初始基線） | 2026-10-09T01:30:46.426Z → 2026-10-09T01:31:01.115Z | 1 / 2 | 0x009F / 0x00A0 / N/A（A 未查） | N/A / N/A | READY / Success; Android READY |
| A02 | 2026-10-09T01:31:31.031Z → 2026-10-09T01:31:38.624Z | 2 / 5 | 0x009F / 0x00A0 / N/A（A 未查） | N/A / N/A | 0x80650003; Android 未訂閱 |
| A03 | 2026-10-09T01:32:06.093Z → 2026-10-09T01:32:17.751Z | 5 / 8 | 0x009F / 0x00A0 / N/A（A 未查） | N/A / N/A | 0x80650003; Android 未訂閱 |
| A04 | 2026-10-09T01:32:27.649Z → 2026-10-09T01:33:01.547Z | 6 / 11 | 0x009F / 0x00A0 / N/A（A 未查） | N/A / N/A | 0x80650003; Android 未訂閱 |
| A05 | 2026-10-09T01:33:11.868Z → 2026-10-09T01:34:04.073Z | 7 / 14 | 0x009F / 0x00A0 / N/A（A 未查） | N/A / N/A | 0x80650003; Android 未訂閱 |
| B01 | 2026-10-09T01:34:37.958Z → 2026-10-09T01:35:07.894Z | 8 / 17 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B02 | 2026-10-09T01:35:37.391Z → 2026-10-09T01:35:45.174Z | 9 / 20 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B03 | 2026-10-09T01:35:59.017Z → 2026-10-09T01:36:06.675Z | 10 / 23 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B04 | 2026-10-09T01:36:21.597Z → 2026-10-09T01:36:29.248Z | 11 / 26 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B05 | 2026-10-09T01:36:43.542Z → 2026-10-09T01:36:51.176Z | 12 / 29 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B06 | 2026-10-09T01:37:05.338Z → 2026-10-09T01:37:13.016Z | 13 / 32 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B07 | 2026-10-09T01:37:28.380Z → 2026-10-09T01:37:36.036Z | 14 / 35 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B08 | 2026-10-09T01:37:47.842Z → 2026-10-09T01:37:55.493Z | 15 / 38 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B09 | 2026-10-09T01:38:06.005Z → 2026-10-09T01:38:13.676Z | 16 / 41 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B10 | 2026-10-09T01:38:27.839Z → 2026-10-09T01:38:35.509Z | 17 / 44 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B11 | 2026-10-09T01:38:47.932Z → 2026-10-09T01:38:55.601Z | 18 / 47 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B12 | 2026-10-09T01:39:06.305Z → 2026-10-09T01:39:13.971Z | 19 / 50 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B13 | 2026-10-09T01:39:28.876Z → 2026-10-09T01:39:36.556Z | 20 / 53 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B14 | 2026-10-09T01:39:50.589Z → 2026-10-09T01:39:58.282Z | 21 / 56 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| B15 | 2026-10-09T01:40:08.928Z → 2026-10-09T01:40:16.615Z | 22 / 59 | 0x009F / 0x00A0 / 0x00A2 | N/A / N/A | READY / Success; Android READY |
| A06 | 2026-10-09T01:40:39.221Z → 2026-10-09T01:40:46.908Z | 23 / 62 | 0x009F / 0x00A0 / N/A（A 未查） | N/A / N/A | 0x80650003; Android 未訂閱 |

每代 `onServiceAdded status=0` 後開始廣播，Stop 返回 OFFLINE。成功輪 descriptor callback `current=true offset=0 prepared=false responseNeeded=true is0200=true`，decision ENABLE / status=0 / sendResponse=true 後 owner READY。失敗輪 generation 沒有新 descriptor-write / characteristic-write callback，未訂閱。candidate/owner snapshot 各輪可能有兩個 BONDED candidates；無法由 callback 的 requestId 或物件 reference 還原 ATT Handle。

## 5. 代表案例與證據索引

### 成功：B01

本次 evidence alias `B01-W` / `B01-D`（原始記錄完成摘要後刪除）。Android generation 17，Windows session [8]。

- 2026-10-09T01:35:04.255Z / session 8 / +1331 ms：`uncached-service-handle` {"count": 1, "handle": "0x009F"}
- 2026-10-09T01:35:04.604Z / session 8 / +1680 ms：`uncached-characteristic-handle` {"count": 1, "handle": "0x00A0"}
- 2026-10-09T01:35:04.605Z / session 8 / +1682 ms：`descriptor-discovery` {"collection_error": null, "duration_ms": 1, "handles": [], "mode": "Cached", "protocol_error": null, "source": "uncached-characteristic", "status": "Ok(GattCommunicationStatus(0))"}
- 2026-10-09T01:35:04.962Z / session 8 / +2038 ms：`descriptor-discovery` {"collection_error": null, "duration_ms": 356, "handles": ["0x00A2"], "mode": "Uncached", "protocol_error": null, "source": "uncached-characteristic", "status": "Ok(GattCommunicationStatus(0))"}
- 2026-10-09T01:35:04.979Z / session 8 / +2055 ms：`descriptor-discovery` {"collection_error": null, "duration_ms": 1, "handles": ["0x00A2"], "mode": "Cached", "protocol_error": null, "source": "cached-characteristic", "status": "Ok(GattCommunicationStatus(0))"}
- 2026-10-09T01:35:05.322Z / session 8 / +2398 ms：`descriptor-discovery` {"collection_error": null, "duration_ms": 342, "handles": ["0x00A2"], "mode": "Uncached", "protocol_error": null, "source": "cached-characteristic", "status": "Ok(GattCommunicationStatus(0))"}
- 2026-10-09T01:35:05.322Z / session 8 / +2399 ms：`cccd-write-begin` {"value": "0200"}
- 2026-10-09T01:35:05.501Z / session 8 / +2577 ms：`cccd-write-end` {"duration_ms": 177, "status": "GattCommunicationStatus(0)"}
- 2026-10-09T01:35:05.502Z / session 8 / +2578 ms：`ready` {}

Android 同測試區間的去識別化 callback：

- `utc=2026-10-09T01:35:04.694740Z elapsed=286325638 g=17 descriptor-write current=true offset=0 prepared=false responseNeeded=true is0200=true candidates=2 owner=false`
- `utc=2026-10-09T01:35:04.705036Z elapsed=286325649 g=17 sendResponse status=0 sent=true`
- `utc=2026-10-09T01:35:04.705546Z elapsed=286325649 g=17 owner READY`

ATT frame no. / actual Write Handle / Android UUID→CCCD mapping：N/A。上述 WinRT descriptor `0x00A2` 不是實際 ATT Write 的證明。

### 失敗：A02

本次 evidence alias `A02-W` / `A02-D`。Android generation 5，Windows session [2]。

- 2026-10-09T01:31:36.267Z / session 2 / +4077 ms：`uncached-service-handle` {"count": 1, "handle": "0x009F"}
- 2026-10-09T01:31:36.416Z / session 2 / +4226 ms：`uncached-characteristic-handle` {"count": 1, "handle": "0x00A0"}
- 2026-10-09T01:31:36.417Z / session 2 / +4227 ms：`cccd-write-begin` {"value": "0200"}
- 2026-10-09T01:31:36.419Z / session 2 / +4229 ms：`cccd-write-end` {"duration_ms": 1, "hresult": "0x80650003"}

- 當 generation 未觀察到 descriptor / characteristic write callback，也未到 READY。

失敗發生在原 CCCD Indicate write，之前 Service / Characteristic query Success；不是本輪的 identity / pairing / AccessDenied / Unreachable 失敗。`0x80650003` 是 [Microsoft 定義的 ATT WRITE_NOT_PERMITTED](https://learn.microsoft.com/en-us/windows/win32/com/com-error-codes-9)，不把通用 timeout/cancellation 前綴當成原因。CCCD API HRESULT 沒有提供本輪 ProtocolError 位元組，填 N/A。Android 沒有 characteristic-write callback，因此不宣稱舊 CCCD Handle 指向唯讀 Characteristic，也不能排除 stack 在 callback 之前拒絕。

### HCI 取得嘗試

- 原設定讀值：global `bluetooth_btsnoop_log_mode` / secure `bluetooth_hci_log` 為 null，persist 屬性查詢空白／權限受限，無法確認 GUI 的原設定。
- 依需求第 5 節詢問是否同意啟用完整 HCI；測試期間未取得同意回覆，因此**沒有更動 HCI 開關，也沒有重開 Bluetooth**。
- 直接讀 `/data/misc/bluetooth/logs/btsnoop_hci.log`：Permission denied；`/sdcard/btsnoop_hci.log`：不存在。不要求 root。
- `adb bugreport` 成功匯出約 41 MB ZIP；沒有獨立 btsnoop / btsnooz 檔，主文字報告含 `BTSNOOP_LOG_SUMMARY`。
- 依 [AOSP 官方流程](https://source.android.google.cn/docs/core/connect/bluetooth/verifying_debugging?hl=en) 使用 [btsnooz.py](https://android.googlesource.com/platform/packages/modules/Bluetooth/+/refs/heads/main/system/tools/scripts/btsnooz.py) 解碼；再以本次 Python parser 遍歷 btsnoop records。共 6,990 frames：HCI Event 5,159、Command 1,831、ACL 0；全部 captured length 少於 original length。沒有任何 ATT PDU、Write Request 或 Error Response；frame 索引 N/A。summary 顯示的 timestamp 另有時區／格式偏移疑慮，未拿來強行對齊連線。
- bugreport 在 A01 已開始後擷取，當輪有額外系統負載；A01 是基線成功樣本，不能當成嚴格沒有測量影響的控制組。
- Windows 無既有可用 tshark/Wireshark；未啟動猜測性的 ETW provider。沒有宣稱無封包等於未送出。

## 6. 判準與下一步

- **INCONCLUSIVE**：缺少同 connection / generation 的 ATT UUID→Handle mapping 與 actual Write Request / Error Response，且兩個 candidate 的來源未解釋。
- Android GATT 重建後真實 handle 是否改變：UNKNOWN。Windows 可見 Service / Characteristic 未變；B WinRT CCCD 為 `0x00A2`。不能把它當作 Android 權威 handle。
- WinRT CCCD write 到何處：UNKNOWN；是否因 stale handle 得 `0x80650003`：UNKNOWN。
- 觀察支持繼續檢查 descriptor cache、WinRT object / discovery lifecycle、查詢時序及 Android stack；不支持已確認 Handle 錯位、NOT_SENT、權限修復或問題已排除。B 成功與 A 失敗是關聯，不是控制單一變項的因果證據。
- 沒有更動或排除 Android permissions / ownership / candidate 競爭／Windows 本機拒絕等其他解釋。失敗沒有進 app callback，使 app ownership rejection 缺少本輪直接證據。
- 下一步：在取得同意後啟用 **完整** HCI 記錄，按裝置要求重開 Bluetooth，重做一輪原流程失敗與一輪 B 成功，捕捉同連線的 discovery UUID→handle、`0x12` Write Request 的 handle + `02 00`、`0x01` Error Response 的 rejected opcode / handle / code，再依 connection handle / connect-disconnect / generation 交叉對齊。先釐清兩個 candidate 的來源。保持核准與 bond，不清 OS cache、不 unpair。證據足夠後才另立修復任務。

## 7. 實際建置檢查

- 插樁 Windows `cargo check`：通過；`cargo test`：28 passed / 0 failed。
- 插樁後第一次 `cargo fmt --check`：失敗，僅新增 trace_stage 呼叫尚未格式化。正式來源撤回後重新檢查，結果在下方清理驗證記錄。
- 插樁 Android `assembleDebug testDebugUnitTest lintDebug`：通過，16 JVM tests / 0 failures / 0 errors；lint 0 Error、6 既有 Warning。
- SDK 未更新；不用更新 dev-docs 的 SDK 內容。本次結論保存在本 memo，不把暫時 Phase B 行為寫成正式產品功能。

## 8. 清理與還原驗證

本 memo 在刪除資料前先落盤。需求 implementation.md 保留；本任務 captures/scripts 的 raw、backup、測試 flag 與腳本只清理本次產生的檔案。舊 `.agent_temp/ble-ptt-auto-reconnect` probes 不動。

- [x] 已保存所有輪次的去識別化結果、成功／失敗的時戳與 Handle 摘要，以及缺少 ATT frame 的原因；memo 可獨立閱讀。
- [x] 兩個正式原始檔逐 byte 恢復備份，恢復前比對本次插樁後雜湊，避免覆蓋其他異動。沒有保留任何 tracing 或 Phase B discovery。
- [x] 還原後 Windows `cargo fmt --check`、`cargo check`、`cargo test` 全部通過；28 tests passed。
- [x] 還原後 Android `assembleDebug testDebugUnitTest lintDebug` 全部通過；16 tests passed，lint 0 Error / 6 既有 Warning。
- [x] 已安裝還原後 APK，移除 app 私有診斷旗標並重新開啟 Android；UI 顯示 OFFLINE／開始廣播，符合測試前狀態。Tauri dev 已重建並執行還原後來源。
- [x] Approved 設定檔前後 SHA-256 相同；未更動 OS bond、配對、OS GATT cache、BLE protocol 或 permissions。
- [x] HCI 開關從未變動，不需要恢復；完整 HCI 的同意與手動啟用仍是下次取得權威封包的前提。
- [x] 已刪除手機本次產生且時間相符的兩個 bugreport 檔及 `/sdcard/ble-alignment-ui.xml`；只處理本次已識別的檔案，未刪除其他 bugreport。
- [x] 本機 captures 本次 89 個檔案與 scripts 本次 7 個檔案已刪除；兩個目錄均為空。刪除前核對絕對路徑與本次檔名清單，沒有使用 git reset / clean。
- [x] 最終 root repo status 只有本 memo 尚未追蹤；Android repo status 空白。沒有保留正式程式異動，也沒有未清理的本機測試資料。

手機系統 logcat 的短期 ring buffer 未全域清空，避免刪除其他診斷；APK 已不再輸出本次插樁日誌。沒有保留供後續重驗的 raw HCI / bugreport 副本。完整 ATT 證據不足是本次明確的未完成驗證項，不表示根因已排除。

清理完成時間：2026-10-09 09:45:20 +08:00。執行者：Codex。原始 HCI／bugreport 留存例外：無。
