# ESP32 pairing firmware

Target: **Seeed XIAO ESP32S3 Sense**, Arduino ESP32 **3.3.12**, USB CDC enabled.
PTT: GPIO1/D0 to GND; Submit: GPIO2/D1 to GND; internal pullups. PDM CLK GPIO42,
DATA GPIO41, 16kHz mono signed16 PCM, 320 samples / 20ms. LED GPIO21 is active low.
Firmware **0.4.0** includes the X1 controls / Combo / Host permission lease and P8AU
AES-256-GCM audio to the existing Security-1 provisioning and persistent pairing.
The pairing blob layout/schema is unchanged; a valid existing formal PAIRED
record is retained during a normal compatible upload. No erase-all is used.

A separate core-0 PDM worker continuously drains idle samples and sends at most
one frame in flight. It never shares mutable Strings or the control socket with
the main loop. Session-derived HKDF / GCM material and the public synthetic packet
are checked by the original prototype crypto self-test. Audio sequence allocation
is consumed even when encryption/send fails; changing a press/Combo segment never
resets the sequence. Raw PTT release / Submit press and permission/ACK expiry
also close the worker gate immediately before send. No plaintext fallback exists.

DOWN requires current Dictation readiness, both buttons released since reconnect,
and a fresh press. COMBO immediately pauses audio; releasing Submit first resumes
the same press with a fresh audio segment; releasing PTT first finishes without
sending. A five-second chord still performs the existing persistent physical reset,
even without a Host. A fresh independent Submit requires Submit readiness, which
is separate from audio readiness. Permission lease expiry rotates the session;
reconnection never resumes a held press.

## Build (no board changes)

From the repository root, with Arduino CLI on PATH:

```powershell
arduino-cli compile --fqbn esp32:esp32:XIAO_ESP32S3:USBMode=hwcdc,CDCOnBoot=default --build-path .agent_temp/esp32-phase4-build esp32/firmware
```

The Windows installation used for development has CLI at
`C:\Program Files\Arduino IDE\resources\app\lib\backend\resources\arduino-cli.exe`.
No additional Arduino library installation is necessary: cJSON, NVS, mbedTLS,
WiFiProv and WiFiUDP come from the installed ESP32 core.
The device ID is read with `esp_read_mac(..., ESP_MAC_WIFI_STA)` independently
of Arduino netif startup. Read errors, all-zero or multicast addresses reject
startup; the ID is formatted as 12 uppercase hex digits.
For this board in Core 3.3.12, `CDCOnBoot=default` means Enabled and
`CDCOnBoot=cdc` means Disabled. Hardware CDC routes `Serial` to the same USB
COM port used for upload. The sketch rejects builds with incompatible USB flags.
USB diagnostic writes use a zero TX wait and may drop output when the host is
not reading. Serial backpressure must not delay control heartbeats or button
cancellation; a Serial monitor is not required for normal operation.

## Flash only after explicit approval

**Uploading overwrites the currently validated prototype firmware.** First boot
without a valid formal PAIRED record clears Wi-Fi settings and any incomplete
formal pairing. The old prototype namespace is not used for authorization.
Do not assume prototype Wi-Fi/pairing will survive switching firmware. Keep the
original prototype sources/build available before upload. Do not erase all flash
or change eFuse settings.

After approval, identify the correct USB board with `arduino-cli board list`, then:

```powershell
arduino-cli upload --port COM5 --fqbn esp32:esp32:XIAO_ESP32S3:USBMode=hwcdc,CDCOnBoot=default --input-dir .agent_temp/esp32-phase4-build esp32/firmware
```

`COM5` was only an enumeration candidate during development; confirm the attached
Sense before upload. Do not upload to a guessed port. If USB upload fails, hold
the board BOOT button, press/release RESET, then release BOOT and enumerate the
USB bootloader port again. Upload the compatible build to that port without
erase-all. If authorization cannot be recovered safely, reset and pair again.

## Pair and recover

1. Start the rebuilt Windows app. Open Settings → Devices → Add Device.
   The selected device owns a GATT session with MaintainConnection enabled.
   Uncached full service discovery initiates the connection, followed by a
   bounded wait for Active, OS pairing, and endpoint discovery restricted to the
   provisioning service UUID. Handles are closed on failure or disconnect.
   Inactive session, unreachable service discovery, or discovery HRESULT
   80070016 permits one retry with fresh handles for the same selected address.
   Security-1 starts after that retry scope and is never transparently replayed.
   Windows pairing uses the prototype's Custom Pairing / ConfirmOnly flow;
   only ProtectionLevelCouldNotBeMet advances from EncryptionAndAuthentication
   to Encryption, then the OS default. Existing bonds are reused. OS pairing
   failure stops before Security-1 and reports native status/level or HRESULT
   in the E06 reason. Pairing does not replace the Security-1 handshake.
   Other WinRT failures report the operation and HRESULT (for example,
   `ble_get_services_failed_hresult_80070005`); endpoint exchange failures also
   identify the endpoint. Diagnostics omit native messages and packet payloads.
   A non-success read/write result includes the numeric GATT status and optional
   ATT protocol error in hex (`att_none` means Windows supplied no ATT code).
   Setup write failures also identify the command, such as `op_HELLO`.
   Firmware setup diagnostics record fixed HELLO/IDENTIFY/STATUS stages without
   payloads. A callback waits at most 1.5 seconds for the shared state mutex;
   `lock_timeout` rejects the request. The main loop releases that mutex before
   its delay, provisioning shutdown and restart.
2. Choose the advertised `PROV_GPT_` suffix. Confirm its flashing LED; release
   both buttons, then press/release PTT within 30 seconds.
3. Scan 2.4 GHz networks or enter a hidden SSID. Windows 5 GHz/Ethernet is allowed
   when it can reach the ESP32 LAN. Confirm sharing before sending credentials.
   Reading a Windows saved password has separate consent and manual fallback.
4. Wait for both persisted endpoints, authenticated FINALIZED and Wi-Fi-only W.
   Choose Enable or Later explicitly, including for the first device.
5. Quit/restart the app and power-cycle the ESP32 separately; verify fresh
   authentication without BLE. A paired device does not reopen BLE on Wi-Fi loss.

For an uncertain commit, hold **both external buttons continuously for five
seconds**, then Add again. Reset first commits a separate RESETTING marker,
clears formal pairing and Wi-Fi settings, then clears that marker. Interrupted
cleanup is continued at boot. Storage errors reject authorization. Forget only
removes the current Windows user's local record and requires physical device reset
before pairing again. It never removes Windows Bluetooth bonds.

LED rhythms: provisioning short pulse each second, Identify 150 ms on/off,
reconnecting 500 ms on/off, authenticated 80 ms per two seconds, reset countdown
80 ms on/off. Confirm rhythms and all PTT/Combo behavior on real hardware.

## Security and implementation boundaries

The shared fixed PoP matches the validated prototype. Its extraction/impersonation
risk is an **accepted personal-use tradeoff**, not a release blocker or a claim of
unique device identity. Wi-Fi pairing keys are independent random values. Windows
protects its entire pairing store with current-user DPAPI; firmware uses ordinary
NVS, without Secure Boot, flash encryption, Owner enrollment, Security-2 or eFuse.
Neither logs passwords, pairing keys or audio.

The historical Phase 2 build was uploaded to the ESP32-S3 on COM5 with user approval on
2026-10-16; esptool verified all written hashes. After correcting the USB flags,
the STA MAC fix was uploaded and Serial captured
`[BOOT] GPTW 0.2.0 68EE8F50EE44 (BLE)` and `[BLE] PROV_GPT_50EE44` on two boots,
matching the flasher's MAC. USB logging and the startup ID are verified;
the lock-scope fix also completed HELLO, IDENTIFY and STATUS handlers in a real
Windows app retry, with each response produced and no lock timeout. The user then
confirmed successful full real-device connection on 2026-10-16, followed by
automatic recovery after fully quitting/reopening the app and after ESP32 power
cycling, without repeating BLE provisioning. Fault scenarios, second-device
behavior and the remaining Phase 2 hardware gates
still require acceptance testing; this successful connection does not establish
NVS power-cut recovery or every failure path.

API references: [Espressif provisioning](https://docs.espressif.com/projects/esp-idf/en/v5.4/esp32/api-reference/provisioning/wifi_provisioning.html),
[Microsoft uncached GATT reads](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.genericattributeprofile.gattcharacteristic.readvalueasync).

## Windows Phase 3 bridge and validation

The App owns one Rust receiver and one CPAL/WASAPI worker, with exclusive Host
UDP 3335 (X1) and 3333 (P8AU); device sources are 3334 and 3336. Playback explicitly
opens CABLE Input at 48kHz, while only the target Dictation WebView origin resolves
and verifies its own CABLE Output deviceId. It never changes Windows default audio
or persistent microphone preferences. Voice and other tabs retain their policy.
Install/repair VB-CABLE yourself from https://vb-audio.com/Cable/ and use Settings
→ Devices → Audio Bridge → Recheck. The App does not distribute/remove the driver.

Audio uses fixed +12dB / peak 0.94 limiting, prototype release (+0.015 per frame),
16→48k linear interpolation, adaptive 2..10 frame jitter with maximum 12 queued
frames. UP/Combo/cancel discards queued output without tail draining. The callback
also verifies the Windows session generation and control deadline. Output failure,
missing capture proof, >700ms gated audio loss, Lock/Sleep or source change cancels
without Send; a new authenticated session and button release are required.

The runtime adds `cpal 0.15.3`, `aes-gcm 0.10.3`, `hkdf 0.12` and Windows desktop /
session event API features; Tauri, Windows crate and Arduino Core versions remain
unchanged. These development instructions cover the dependency/routing changes;
there is no separate SDK/dev-docs package to update. API references:
https://docs.rs/cpal/0.15.3/cpal/ and https://docs.rs/aes-gcm/0.10.3/aes_gcm/.

Engineering checks do not prove speech capture. The Phase 3 validation record is
`.agent_temp/esp32-product-integration-requirements/phase-3-validation.md`.
Firmware 0.3.0 was uploaded to the verified COM5 board (MAC 68EE8F50EE44) with explicit approval on 2026-10-16. All written hashes verified; boot reported WIFI_ONLY, crypto self-test PASS and PDM ready. No erase-all or physical NVS reset was performed. The rebuilt app was started and owns UDP 3335/3333. **The user confirmed real App × ESP32 × ChatGPT speech transcription and Submit PASS on 2026-10-16.** Remaining acceptance scenarios, including Combo and fault recovery, are pending; this is not full Phase 3 sign-off.
Do not interpret the historical Phase 2 connection report as Phase 3 PASS.

## Phase 4 lifecycle / recovery

Firmware 0.4.0 does not change pairing NVS schema or the X1/P8 wire protocol.
The LED is active-low: provisioning 200ms each second, identify 150ms/300ms,
reconnecting two 100ms pulses per 1500ms, authenticated idle 80ms/2000ms,
audio gate plus PDM ready continuously lit, Combo 80ms/160ms, fatal 100ms/200ms.
The gate light indicates authorized audio transport, not guaranteed ChatGPT capture
or transcript quality. Validate actual polarity/patterns on the board.

Only a still-uncommitted encrypted BLE owner session can use `WIFI_RETRY` after
failed Wi-Fi provisioning. It resets the SDK failed provisioning state, then returns
to Wi-Fi entry; the user must confirm credentials again. PAIRED/PREPARED/WAIT_FINAL
cannot use this command. Host authentication failure requires a fresh session;
uncertain commit requires physical reset. An existing paired Wi-Fi/Host can only
be changed by holding both buttons five seconds and pairing again.

The Windows Receiver retries with backoff and pauses after three consecutive
failures; stop the conflicting program and choose Devices → Retry. Audio driver
failures require Audio Bridge → Recheck. A reconnect never resumes a held PTT.
Forget removes only this user's local record; the board still needs physical reset.
USB repair: upload a compatible build without erase-all; if needed hold BOOT,
tap RESET, release BOOT and re-enumerate the bootloader COM port. Preserve NVS
where compatible; if credentials are lost, reset and pair again. The App's Devices
page includes these recovery steps. Hardware fault testing is tracked separately
in `.agent_temp/esp32-product-integration-requirements/phase-4-validation.md`.
No SDK/dependency versions changed for Phase 4; these development docs and the
protocol contract were updated for the added command and lifecycle behavior.

On 2026-10-16 the user explicitly approved uploading 0.4.0. COM5 was re-enumerated
and verified as ESP32-S3 MAC 68EE8F50EE44 before upload. Every written hash passed;
boot reported 0.4.0 WIFI_ONLY, crypto self-test PASS and PDM ready. Existing pairing
was retained and the final Windows App authenticated the device with Audio Ready.
Receiver port-conflict pause/manual retry and idle Tray Quit/port release/reopen
were verified with the user. LED, live PTT fault handling and the remaining Phase 4
UAT are separate pending checks, not implied by this compatible USB update.
