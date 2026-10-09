# BLE CCCD 重連 A/B/C 控制變因測試 — Delay vs Discovery Memo

日期：2026-10-09（Asia/Taipei，UTC+08:00）
狀態：**已執行30次Stop/Start；root cause INCONCLUSIVE；清理完成。**
需求：`.agent_temp/ble-cccd-delay-vs-discovery/implementation.md`。
上一輪：`memo/2026-10-09-ble-handle-alignment-diagnosis.md`。

## 1. 結論與證據界線

本輪A沒有重現上一輪0x80650003。即使B/C成功，也不能認定700ms延遲或單次Descriptor Discovery解決問題；沒有可歸因的失敗基線。根因未確認，也未排除cache/carryover/時序問題，正式產品修復另案。

Android真實ATT CCCD Handle、實際ATT Write Request Handle、同link HCI mapping均N/A。WinRT Descriptor Handle只描述Windows API查得屬性，不是實際ATT Write Handle。

## 2. 環境與控制

- Windows 11 Pro build 26200、TP-Link Bluetooth USB Adapter；原始commit `983dc285b0af553eb34f9082541c3b64104e0c04`。
- Samsung SM-S9160、Android 16、debug APK v0.1.0；Android獨立repo commit `ea84f902a4c7272d6b5185716c83cc85e5680d61`。
- 起始root status只有本次與上一輪兩份memo未追蹤；Android乾淨。修改前保存Rust/Kotlin原始bytes與SHA256。未覆蓋其他未提交修改。
- Windows Enabled=true、approved ID已存在；各connect皆經原should_connect gate確認approved identity match與IsPaired=true。不操作Scan/Approve/Forget、不清cache、不重新配對。核准設定以SHA256前後比對；不將ID、MAC或配對秘密寫入memo。
- 插樁建置使既有tauri dev在preflight前自動重建/重啟；Android插樁APK也在preflight前更新安裝、回到OFFLINE。正式R01–R30維持同一Windows PID，每輪前後檢查；沒有因切換模式重啟App，沒有新增獨立BLE Central或P5 probe。程序檢查只見一個正式gpt-wrapper.exe。
- Android保持前景；ADB uiautomator讀取當前開始/停止廣播按鈕文字與bounds，只操作該兩個按鈕。未操作PTT/Submit、Bluetooth開關或HCI snoop。原HCI設定沒有重新取樣（N/A），也沒有更動。
- 忽略目錄`captures/mode.json`保存round/mode；每次connect固定snapshot。Android私有`files/ble-cccd-diagnosis-enabled`控制UTC、elapsedRealtime、generation、candidates、owner的opt-in log。
- A無新增等待/遠端查詢；B在既有worker sleep700ms；C對原Uncached Characteristic只做一次`GetDescriptorsForUuidWithCacheModeAsync(0x2902, Uncached)`，不補sleep、不增加Cached/Service/Char查詢。C query outcome與write outcome分開；query失敗本身不阻斷有效連線。B/C之後檢查invalidated與ConnectionStatus，避免寫失效連線。
- 三組保留原`WriteClientCharacteristicConfigurationDescriptorAsync(Indicate)`、handler、worker/backoff、UUID、CCCD permissions、ownership和BLE lifecycle。
- Android每代兩筆BONDED candidate connection callback、最大candidate count=2。沒有HCI link身分，不能把它們視為同link，也不能單憑時間把Android owner映射為指定Windows session。Windows attempt結果有直接session證據；Android欄位僅同generation旁證，link mapping=N/A。未觀察到競爭write，但不能宣稱已排除另一Central。
- 共通JSONL寫盤與Android opt-in log略改變時序；preflight前的dev重啟/更新APK、既有cache與Stop→Start間隔也使本次與上一輪條件未必相等。未變更SDK，dev-docs無需更新。

## 3. 排程與摘要

初始OFFLINE→Start preflight：A成功、Windows session=1、Android generation=2，不計入Stop/Start。正式10個block維持ABC / BCA / CAB / CBA / BAC / ACB / ABC / BCA / CAB / CBA；不依成功率改順序。

每輪先讀worker/candidates狀態，Stop後以新server closed log和Start按鈕確認OFFLINE；等前一attempt完成，使用相同至少3秒等待規則，再切mode/round並Start。UI dump/logcat另有耗時，實際Stop→Start並非精確3秒，逐輪记录。Start後最多90秒等首個connect-end，再同樣觀察12秒保存retry與READY。Eventual READY僅指此窗口，不代表無限期追蹤。沒有調整cooldown、補選成功輪或把retry當新增Stop/Start。

| Group | Stop/Start | 時間介入可比輪數 | Connect attempts | First success（全部觀察輪） | 0x80650003 | Other/incomplete | Eventual READY |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| A | 10 | 10 | 10 | 10 | 0 | 0 | 10 |
| B | 10 | 9 | 10 | 10 | 0 | 0 | 10 |
| C | 10 | 10 | 10 | 10 | 0 | 0 | 10 |

| Group | Char→Write min/median/max (ms) | B sleep min/median/max (ms) | C query min/median/max (ms) | connect→Service完成 (ms) | Service→Char完成 (ms) |
| --- | --- | --- | --- | --- | --- |
| A | 1.435 / 1.720 / 2.225 | N/A | N/A | 928.545 / 2269.273 / 9281.100 | 121.106 / 7770.103 / 8339.618 |
| B | 702.078 / 702.647 / 1343.502 | 700.369 / 700.676 / 701.010 | N/A | 931.181 / 4846.571 / 8870.606 | 120.892 / 3970.345 / 8079.958 |
| C | 117.547 / 118.664 / 138.558 | N/A | 115.768 / 116.771 / 137.052 | 915.224 / 1692.441 / 9637.461 | 141.005 / 7919.848 / 8100.778 |

Stop→Start min/median/max=9881.374 / 12674.310 / 17413.936 ms。後兩欄為實際phase marker間隔；connect→Service含device open/check_radio，Service→Char含狀態更新，不是各WinRT API獨立stopwatch。Char→Write、sleep、C query則有直接stopwatch。

正式觀察區間（首輪Stop至末輪保存）：2026-10-09T02:22:15.420Z → 2026-10-09T02:47:14.420Z。

截至末輪保存，包含preflight→R01轉換的Lost事件：{"ServiceChanged": 30}；總retry事件=0。Stop附近Lost與下一輪attempt分開，不當成新CCCD失敗。


## 4. 每輪結果

所有Android ATT CCCD/Write Handle均N/A。A/B Descriptor未查，N/A。C的ProtocolError=null不是CCCD write的ProtocolError：原write API只回GattStatus或HRESULT，write ProtocolError為N/A。UTC是Windows connect開始；Android時鐘偏差本輪沒有獨立量測，兩端monotonic時鐘也不能直接比較。

| Block/Round | Group | UTC connect開始 | Win session/Android generation | Service/Char/C Descriptor | C status/protocol/count | B sleep/C query (ms) | Char→Write (ms) | CCCD result/write ms | Android 0200/READY/candidates | retry/gate failures | 可比性 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1/R01 | A | 2026-10-09T02:22:27.824Z | 2/5 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.541 | SUCCESS/55.528 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 1/R02 | B | 2026-10-09T02:23:16.097Z | 3/8 | 0x009F/0x00A0/N/A | N/A（未查） | 700.369 | 702.089 | SUCCESS/94.603 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 1/R03 | C | 2026-10-09T02:24:07.547Z | 4/11 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 136.919 | 138.381 | SUCCESS/58.276 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 2/R04 | B | 2026-10-09T02:24:59.510Z | 5/14 | 0x009F/0x00A0/N/A | N/A（未查） | 700.684 | 702.293 | SUCCESS/74.067 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 2/R05 | C | 2026-10-09T02:25:54.099Z | 6/17 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 116.700 | 118.136 | SUCCESS/58.372 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 2/R06 | A | 2026-10-09T02:26:40.929Z | 7/20 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.636 | SUCCESS/74.902 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 3/R07 | C | 2026-10-09T02:27:39.267Z | 8/23 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 116.754 | 118.208 | SUCCESS/78.154 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 3/R08 | A | 2026-10-09T02:28:24.428Z | 9/26 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.732 | SUCCESS/74.640 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 3/R09 | B | 2026-10-09T02:29:13.861Z | 10/29 | 0x009F/0x00A0/N/A | N/A（未查） | 700.668 | 1343.502 | SUCCESS/113.058 | True/True/2 | 0/0 | INELIGIBLE於700ms劑量比較；Android link N/A |
| 4/R10 | C | 2026-10-09T02:30:04.031Z | 11/32 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 137.052 | 138.558 | SUCCESS/58.175 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 4/R11 | B | 2026-10-09T02:30:56.015Z | 12/35 | 0x009F/0x00A0/N/A | N/A（未查） | 700.655 | 702.846 | SUCCESS/73.452 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 4/R12 | A | 2026-10-09T02:31:49.600Z | 13/38 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.476 | SUCCESS/75.709 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 5/R13 | B | 2026-10-09T02:32:37.387Z | 14/41 | 0x009F/0x00A0/N/A | N/A（未查） | 700.969 | 702.604 | SUCCESS/73.864 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 5/R14 | A | 2026-10-09T02:33:20.523Z | 15/44 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.435 | SUCCESS/74.717 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 5/R15 | C | 2026-10-09T02:34:13.658Z | 16/47 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 116.570 | 118.262 | SUCCESS/97.534 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 6/R16 | A | 2026-10-09T02:35:01.039Z | 17/50 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 2.124 | SUCCESS/74.099 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 6/R17 | C | 2026-10-09T02:35:59.116Z | 18/53 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 115.768 | 117.547 | SUCCESS/78.197 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 6/R18 | B | 2026-10-09T02:36:50.910Z | 19/56 | 0x009F/0x00A0/N/A | N/A（未查） | 701.010 | 703.081 | SUCCESS/93.397 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 7/R19 | A | 2026-10-09T02:37:37.354Z | 20/59 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 2.225 | SUCCESS/74.299 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 7/R20 | B | 2026-10-09T02:38:26.214Z | 21/62 | 0x009F/0x00A0/N/A | N/A（未查） | 700.402 | 702.078 | SUCCESS/74.080 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 7/R21 | C | 2026-10-09T02:39:15.817Z | 22/65 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 116.788 | 118.658 | SUCCESS/77.643 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 8/R22 | B | 2026-10-09T02:40:04.123Z | 23/68 | 0x009F/0x00A0/N/A | N/A（未查） | 700.600 | 702.315 | SUCCESS/93.999 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 8/R23 | C | 2026-10-09T02:40:55.339Z | 24/71 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 136.253 | 138.036 | SUCCESS/58.240 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 8/R24 | A | 2026-10-09T02:41:43.252Z | 25/74 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.780 | SUCCESS/74.617 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 9/R25 | C | 2026-10-09T02:42:36.506Z | 26/77 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 116.496 | 118.670 | SUCCESS/97.860 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 9/R26 | A | 2026-10-09T02:43:29.516Z | 27/80 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.903 | SUCCESS/94.217 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 9/R27 | B | 2026-10-09T02:44:22.864Z | 28/83 | 0x009F/0x00A0/N/A | N/A（未查） | 700.922 | 702.691 | SUCCESS/74.445 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 10/R28 | C | 2026-10-09T02:45:13.624Z | 29/86 | 0x009F/0x00A0/0x00A2 | Ok(GattCommunicationStatus(0))/null/1 | 136.208 | 138.112 | SUCCESS/77.838 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 10/R29 | B | 2026-10-09T02:46:02.036Z | 30/89 | 0x009F/0x00A0/N/A | N/A（未查） | 701.003 | 702.702 | SUCCESS/73.595 | True/True/2 | 0/0 | Windows內可比；Android link N/A |
| 10/R30 | A | 2026-10-09T02:46:53.122Z | 31/92 | 0x009F/0x00A0/N/A | N/A（未查） | N/A | 1.709 | SUCCESS/54.961 | True/True/2 | 0/0 | Windows內可比；Android link N/A |

| Round | Stop→Start ms | Start→attempt ms | connect→Service ms | Service→Char ms | gate剩餘cooldown ms | attempts/窗內READY |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| R01 | 11016.368 | 1386.690 | 1260.837 | 7779.949 | 0 | 1/True |
| R02 | 12599.816 | 682.468 | 3150.477 | 7859.811 | 0 | 1/True |
| R03 | 14629.287 | 595.960 | 1202.590 | 7739.879 | 0 | 1/True |
| R04 | 10399.991 | 663.420 | 7219.914 | 121.433 | 0 | 1/True |
| R05 | 16863.252 | 118.308 | 915.224 | 7939.933 | 0 | 1/True |
| R06 | 11560.360 | 2480.714 | 8685.883 | 121.106 | 0 | 1/True |
| R07 | 16603.624 | 877.493 | 1291.529 | 8059.970 | 0 | 1/True |
| R08 | 9921.669 | 1040.620 | 6750.717 | 141.049 | 0 | 1/True |
| R09 | 10447.413 | 265.364 | 7599.806 | 121.014 | 0 | 1/True |
| R10 | 15283.012 | 785.066 | 2093.352 | 7899.764 | 0 | 1/True |
| R11 | 13090.078 | 910.095 | 931.181 | 7920.245 | 0 | 1/True |
| R12 | 15915.097 | 1279.149 | 928.545 | 8339.618 | 0 | 1/True |
| R13 | 9983.491 | 1824.893 | 6542.664 | 120.892 | 0 | 1/True |
| R14 | 11638.260 | 767.496 | 1329.647 | 7940.569 | 0 | 1/True |
| R15 | 12748.804 | 234.732 | 936.370 | 8100.778 | 0 | 1/True |
| R16 | 11237.529 | 79.484 | 2937.415 | 8040.366 | 0 | 1/True |
| R17 | 16400.133 | 1799.415 | 1143.608 | 8040.781 | 0 | 1/True |
| R18 | 14732.999 | 379.116 | 8870.606 | 121.013 | 0 | 1/True |
| R19 | 10390.881 | 76.618 | 7908.340 | 141.196 | 0 | 1/True |
| R20 | 13422.500 | 601.801 | 1112.071 | 7880.096 | 0 | 1/True |
| R21 | 9881.374 | 467.254 | 7469.267 | 160.925 | 0 | 1/True |
| R22 | 13452.691 | 710.816 | 1906.242 | 8079.958 | 0 | 1/True |
| R23 | 10939.800 | 988.346 | 2232.409 | 7980.136 | 0 | 1/True |
| R24 | 12569.654 | 1550.786 | 1601.131 | 7760.257 | 0 | 1/True |
| R25 | 11758.289 | 356.092 | 8789.026 | 141.005 | 0 | 1/True |
| R26 | 13344.691 | 441.979 | 9281.100 | 121.665 | 0 | 1/True |
| R27 | 11961.982 | 1820.495 | 1356.608 | 7819.257 | 0 | 1/True |
| R28 | 17413.936 | 779.385 | 9637.461 | 141.512 | 0 | 1/True |
| R29 | 14938.876 | 77.578 | 8727.835 | 120.956 | 0 | 1/True |
| R30 | 12818.912 | 746.622 | 984.514 | 7979.955 | 0 | 1/True |


gate剩餘cooldown=0只表示原gate允許該attempt開始，不能反推Start時從未等待；Start→attempt另列。Windows內first-attempt結果可確認，Android跨link歸因仍受兩candidate限制，不列為已確認的同link樣本。時間介入可比輪數另排除R09等明顯本地停頓；表中success仍完整呈現所有觀察輪，沒有刪掉原數據或補選成功。

## 5. 原始代表案例（去識別化）

本輪A未失敗，沒有本輪A失敗實例可列。上一輪A02是歷史證據，見上一輪memo，不能算入本輪。

### A / R01 / SUCCESS

Windows session=2，Android generation=5；Char→Write=1.541 ms。

```text
2026-10-09T02:22:36.865Z session=2 +9040.786ms char-discovery-finished {"count": 1, "handle": 160}
2026-10-09T02:22:36.865Z session=2 +9041.155ms handlers-finished {}
2026-10-09T02:22:36.866Z session=2 +9041.435ms additional-begin {}
2026-10-09T02:22:36.866Z session=2 +9041.891ms additional-end {"duration_ms": 0.4564}
2026-10-09T02:22:36.867Z session=2 +9042.316ms cccd-write-begin {"char_to_write_ms": 1.5409}
2026-10-09T02:22:36.922Z session=2 +9097.866ms cccd-write-end {"duration_ms": 55.5282, "hresult": null, "status": "GattCommunicationStatus(0)"}
2026-10-09T02:22:36.923Z session=2 +9098.269ms ready {}
2026-10-09T02:22:36.923Z session=2 +9098.573ms connect-end {"error": null, "ready": true}
utc=2026-10-09T02:22:36.064492Z elapsed=289177008 g=5 candidates=2 owner=false descriptor-write is0200=true offset=0 prepared=false responseNeeded=true role=candidate bond=BONDED
utc=2026-10-09T02:22:36.073612Z elapsed=289177017 g=5 candidates=2 owner=true owner READY；有效 CCCD Indicate 已訂閱
```

### B / R02 / SUCCESS

Windows session=3，Android generation=8；Char→Write=702.089 ms。

```text
2026-10-09T02:23:27.107Z session=3 +11010.288ms char-discovery-finished {"count": 1, "handle": 160}
2026-10-09T02:23:27.108Z session=3 +11010.685ms handlers-finished {}
2026-10-09T02:23:27.108Z session=3 +11011.000ms additional-begin {}
2026-10-09T02:23:27.808Z session=3 +11711.379ms delay-end {"duration_ms": 700.3686}
2026-10-09T02:23:27.809Z session=3 +11711.871ms additional-end {"duration_ms": 700.8721999999999}
2026-10-09T02:23:27.809Z session=3 +11712.368ms cccd-write-begin {"char_to_write_ms": 702.0893}
2026-10-09T02:23:27.904Z session=3 +11806.984ms cccd-write-end {"duration_ms": 94.60340000000001, "hresult": null, "status": "GattCommunicationStatus(0)"}
2026-10-09T02:23:27.904Z session=3 +11807.410ms ready {}
2026-10-09T02:23:27.905Z session=3 +11807.752ms connect-end {"error": null, "ready": true}
utc=2026-10-09T02:23:27.012919Z elapsed=289227957 g=8 candidates=2 owner=false descriptor-write is0200=true offset=0 prepared=false responseNeeded=true role=candidate bond=BONDED
utc=2026-10-09T02:23:27.024384Z elapsed=289227968 g=8 candidates=2 owner=true owner READY；有效 CCCD Indicate 已訂閱
```

### C / R03 / SUCCESS

Windows session=4，Android generation=11；Char→Write=138.381 ms。

```text
2026-10-09T02:24:16.489Z session=4 +8942.469ms char-discovery-finished {"count": 1, "handle": 160}
2026-10-09T02:24:16.490Z session=4 +8942.837ms handlers-finished {}
2026-10-09T02:24:16.490Z session=4 +8943.190ms additional-begin {}
2026-10-09T02:24:16.627Z session=4 +9080.129ms query-end {"collection_error": null, "duration_ms": 136.9195, "handles": [162], "protocol_error": null, "status": "Ok(GattCommunicationStatus(0))"}
2026-10-09T02:24:16.627Z session=4 +9080.514ms additional-end {"duration_ms": 137.325}
2026-10-09T02:24:16.628Z session=4 +9080.840ms cccd-write-begin {"char_to_write_ms": 138.3809}
2026-10-09T02:24:16.686Z session=4 +9139.137ms cccd-write-end {"duration_ms": 58.2756, "hresult": null, "status": "GattCommunicationStatus(0)"}
2026-10-09T02:24:16.686Z session=4 +9139.537ms ready {}
2026-10-09T02:24:16.687Z session=4 +9139.848ms connect-end {"error": null, "ready": true}
utc=2026-10-09T02:24:15.821437Z elapsed=289276765 g=11 candidates=2 owner=false descriptor-write is0200=true offset=0 prepared=false responseNeeded=true role=candidate bond=BONDED
utc=2026-10-09T02:24:15.824730Z elapsed=289276768 g=11 candidates=2 owner=true owner READY；有效 CCCD Indicate 已訂閱
```

### B / R09 / SUCCESS

Windows session=10，Android generation=29；Char→Write=1343.502 ms。

```text
2026-10-09T02:29:21.582Z session=10 +7720.821ms char-discovery-finished {"count": 1, "handle": 160}
2026-10-09T02:29:21.582Z session=10 +7721.213ms handlers-finished {}
2026-10-09T02:29:21.583Z session=10 +7721.621ms additional-begin {}
2026-10-09T02:29:22.283Z session=10 +8422.301ms delay-end {"duration_ms": 700.6685}
2026-10-09T02:29:22.925Z session=10 +9063.636ms additional-end {"duration_ms": 1342.0056}
2026-10-09T02:29:22.925Z session=10 +9064.311ms cccd-write-begin {"char_to_write_ms": 1343.5016}
2026-10-09T02:29:23.039Z session=10 +9177.391ms cccd-write-end {"duration_ms": 113.0583, "hresult": null, "status": "GattCommunicationStatus(0)"}
2026-10-09T02:29:23.039Z session=10 +9177.905ms ready {}
2026-10-09T02:29:23.040Z session=10 +9178.383ms connect-end {"error": null, "ready": true}
utc=2026-10-09T02:29:22.147450Z elapsed=289583091 g=29 candidates=2 owner=false descriptor-write is0200=true offset=0 prepared=false responseNeeded=true role=candidate bond=BONDED
utc=2026-10-09T02:29:22.157474Z elapsed=289583101 g=29 candidates=2 owner=true owner READY；有效 CCCD Indicate 已訂閱
```


以上UTC/monotonic/phase/HRESULT/Status及Android callback片段可在raw log清理後獨立閱讀。若沒有本輪A失敗，不能拿歷史失敗替代。

## 6. 判讀與下一步

A不再失敗，本輪INCONCLUSIVE；無法區分延遲或Discovery機制。C若快於700ms仍不是因果證據，因為A也成功。既定交錯block無法消除既有cache/carryover、兩candidate、discovery前置耗時及插樁影響。

R09的B sleep=700.669ms，但Char→Write=1343.502ms；delay-end marker後至additional-end marker約641.335ms。這段在本地telemetry/排程區間，沒有新增GATT query；原因未量測，不能認定是sleep本身延長。此輪只適合記為B配置結果，不當成精確700ms劑量的可比樣本。表中時間可比輪數是事後品質標註，並非預先註冊的統計排除；超過sleep 50ms者保留原結果並標限制，不能據此挑選或補做成功輪。寫入begin marker位於原API前，marker的JSONL append亦有未獨立量測耗時；write duration含此append，並非純ATT往返時間。這項插樁限制一併保留。

下一步先在無插樁原流程、相同配對/approved identity下再次取得可重現失敗的基線。基線穩定後，再考慮C實測耗時匹配的純Delay對照；本輪不增設第四組，不保留delay/query當修復。要確認stale Handle，另案取得完整Android HCI/ATT discovery與實際Write/Error Response同link證據；C全成功也不足以CONFIRMED。

[Microsoft HRESULT定義](https://learn.microsoft.com/en-us/windows/win32/com/com-error-codes-9)：0x80650003=ATT WRITE_NOT_PERMITTED，並非timeout，也不是stale Handle的證明。phase-scoped log保留原write結果，不以既有泛用timeout前綴解讀錯誤。

## 7. 建置與清理前狀態

- 插樁後Windows cargo fmt --check / check / test通過；28 tests。
- 插樁後Android assembleDebug / testDebugUnitTest / lintDebug通過；16 JVM tests、lint既有6個warning。
- 清理前root tracked diff只有Rust診斷；Android只有Kotlin opt-in log。兩份原有未追蹤memo保留。
- 按需求先保存本memo，再清理本次captures/scripts、逐byte還原来源，最後補記還原後驗證。

## 8. 清理 Checklist

- [x] 完整記錄實際輪數、交錯順序、first-attempt結果、真實耗時及證據限制
- [x] Memo已保存去識別化結果與代表案例，可獨立於raw log閱讀
- [x] 本次captures raw log、JSON/CSV、mode及backup已清理
- [x] 本次scripts已清理
- [x] Rust/Kotlin臨時插樁、delay/query已逐byte還原且SHA256相符
- [x] 還原後Windows與Android建置/測試通過
- [x] Android opt-in檔與本次UI XML已移除，原版APK重新安裝並回到OFFLINE
- [x] 核准設定SHA256未變；未呼叫cache清除、OS bond/HCI變更
- [x] 兩repo最終status已確認；兩輪memo及本implementation保留，舊測試/probes未動

清理完成時間：2026-10-09T10:50:50+08:00（Asia/Taipei）。
還原後驗證：Windows cargo fmt --check、cargo check、cargo test 全數通過（28 tests）；Android assembleDebug、testDebugUnitTest、lintDebug 全數通過（16 tests，既有6個lint warning）。兩個正式原始檔逐byte恢復且SHA256相符，核准設定SHA256在清理及建置後均未變。原版debug APK重新安裝成功，UI確認OFFLINE；以run-as直接ls確認opt-in檔不存在，本次UI XML也已刪除。

最終root git status僅：

```text
?? memo/2026-10-09-ble-cccd-delay-vs-discovery-diagnosis.md
?? memo/2026-10-09-ble-handle-alignment-diagnosis.md
```

Android git status空白；兩repo無tracked source diff。本次任務資料夾只保留implementation.md，兩輪memo與舊測試/probes未刪除。
未清理例外：本次captures/scripts及所有獨立raw檔、mode、backup均已移除；沒有額外保存原始證據。Android系統既有logcat環形緩衝區未作全域清除，以免刪除其他既有診斷；本次opt-in trace可能保留至OS自然輪替，未另存檔。本次沒有變更HCI/OS bond設定或呼叫GATT cache清除。
