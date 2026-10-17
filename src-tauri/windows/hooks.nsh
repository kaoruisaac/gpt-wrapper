!ifndef GPTW_ESP32_CLEANUP
!define GPTW_ESP32_CLEANUP
!include LogicLib.nsh

; Only these files belong to the ESP32 pairing store. Never recurse into the
; profile, remove OS bonds/drivers, or delete ble-controller.json/preferences.
; The macro accepts a directory so the identical deletion can be tested in an
; isolated fixture. Production always supplies current-user roaming AppData.
!macro GPTW_REMOVE_PAIRING_FILES CONFIG_DIR FAILED
  StrCpy ${FAILED} 0
  ; A missing directory is harmless, but inability to inspect an existing
  ; directory must never be mistaken for an empty, successfully cleaned store.
  System::Call 'kernel32::GetFileAttributesW(w "${CONFIG_DIR}") i .r6 ?e'
  Pop $5
  ${If} $6 = -1
    ${If} $5 <> 2
    ${AndIf} $5 <> 3
      StrCpy ${FAILED} 1
    ${EndIf}
  ${Else}
    IntOp $6 $6 & 0x10
    ${If} $6 = 0
      StrCpy ${FAILED} 1
    ${Else}
      ClearErrors
      FindFirst $R8 $R9 "${CONFIG_DIR}\*"
      ${If} ${Errors}
        StrCpy ${FAILED} 1
      ${Else}
        FindClose $R8
      ${EndIf}
    ${EndIf}
  ${EndIf}
  ClearErrors
  Delete "${CONFIG_DIR}\esp32-pairs.dpapi"
  IfErrors 0 +2
    StrCpy ${FAILED} 1
  ClearErrors
  Delete "${CONFIG_DIR}\.esp32-*.tmp"
  IfErrors 0 +2
    StrCpy ${FAILED} 1
  ; Verify absence, including unexpected directories at these names.
  IfFileExists "${CONFIG_DIR}\esp32-pairs.dpapi" 0 +2
    StrCpy ${FAILED} 1
  FindFirst $R8 $R9 "${CONFIG_DIR}\.esp32-*.tmp"
  StrCmp $R9 "" +2
    StrCpy ${FAILED} 1
  FindClose $R8
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    ; Stop the writer before deleting keys. Abort leaves the uninstaller in
    ; place for retry; no file is scheduled for deletion on a later reboot.
    !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
    SetShellVarContext current
    !insertmacro GPTW_REMOVE_PAIRING_FILES "$APPDATA\${BUNDLEID}" $R7
    ${If} $R7 <> 0
      DetailPrint "E23: ESP32 pairing data may remain. Cleanup did not complete."
      ClearErrors
      FileOpen $R8 "$TEMP\${BUNDLEID}-esp32-uninstall-E23.log" w
      ${IfNot} ${Errors}
        FileWrite $R8 'E23: ESP32 pairing cleanup failed. Quit GPT Wrapper and retry uninstall as the same Windows user. Check permissions and remove only esp32-pairs.dpapi and .esp32-*.tmp in "$APPDATA\${BUNDLEID}". Do not remove ble-controller.json, other users data, Bluetooth bonds or VB-CABLE. Reset both device buttons for five seconds before re-pairing.$\r$\n'
        FileClose $R8
      ${EndIf}
      MessageBox MB_OK|MB_ICONSTOP 'E23: ESP32 pairing data may remain. Uninstall stopped. Quit GPT Wrapper and retry as this Windows user. Check permissions for "$APPDATA\${BUNDLEID}" and remove only esp32-pairs.dpapi and .esp32-*.tmp. See "$TEMP\${BUNDLEID}-esp32-uninstall-E23.log" if writable. Reset the device with both buttons held five seconds before re-pairing.' /SD IDOK
      SetErrorLevel 23
      Quit
    ${EndIf}
    DetailPrint "Current-user ESP32 pairing files removed. Reset device before re-pairing."
  ${EndIf}
!macroend
!endif
