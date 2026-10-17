param(
    [string]$Makensis = "$env:LOCALAPPDATA\tauri\NSIS\Bin\makensis.exe"
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$fixtureId = 'gptw-installer-test-' + [guid]::NewGuid().ToString('N')
$fixture = Join-Path ([Environment]::GetFolderPath('ApplicationData')) $fixtureId
$scratch = Join-Path ([IO.Path]::GetTempPath()) $fixtureId
$utils = Join-Path $repo 'src-tauri\target\release\nsis\x64\utils.nsh'
if (!(Test-Path -LiteralPath $utils)) { throw 'Build the NSIS installer first (npm run tauri -- build --bundles nsis).' }
New-Item -ItemType Directory -Path $fixture, $scratch | Out-Null
$lock = $null
$fixtureAcl = $null
try {
    $source = @"
Unicode true
RequestExecutionLevel user
SilentInstall silent
!include MUI2.nsh
!include FileFunc.nsh
!include x64.nsh
!include "Win\COM.nsh"
!include "Win\Propkey.nsh"
!include "Win\RestartManager.nsh"
!include "$utils"
!addplugindir "$env:LOCALAPPDATA\tauri\NSIS\Plugins\x86-unicode\additional"
!include "$repo\src-tauri\windows\hooks.nsh"
!define MAINBINARYNAME "nonexistent-fixture-app"
!define PRODUCTNAME "GPTW Installer Fixture"
!define BUNDLEID "$fixtureId"
Var UpdateMode
Var PassiveMode
Name "GPTW Installer Fixture"
OutFile "$scratch\probe.exe"
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
LangString appRunning 1033 "Fixture is running"
LangString appRunningOkKill 1033 "Stop fixture"
LangString failedToKillApp 1033 "Cannot stop fixture"
Section
  StrCpy `$PassiveMode 0
  StrCpy `$INSTDIR "$scratch"
  `${GetOptions} `$CMDLINE "/UPDATE" `$R0
  `${IfNot} `${Errors}
    StrCpy `$UpdateMode 1
  `${EndIf}
  !insertmacro NSIS_HOOK_PREUNINSTALL
SectionEnd
"@
    $nsi = Join-Path $scratch 'probe.nsi'
    [IO.File]::WriteAllText($nsi, $source)
    & $Makensis /V2 $nsi
    if ($LASTEXITCODE -ne 0) { throw 'NSIS fixture compile failed' }
    $preserved = @('ble-controller.json', 'general-settings.json', 'project-settings.json', 'shortcut-settings.json', 'unrelated.tmp')
    foreach ($name in $preserved) { [IO.File]::WriteAllText((Join-Path $fixture $name), 'fixture-preserve') }
    $key = Join-Path $fixture 'esp32-pairs.dpapi'
    $temp = Join-Path $fixture '.esp32-fixture.tmp'
    [IO.File]::WriteAllText($key, 'synthetic-not-a-key')
    [IO.File]::WriteAllText($temp, 'synthetic-not-a-key')
    function Invoke-Fixture([string]$mode, [int]$expected) {
        $argsForProbe = @('/S')
        if ($mode) { $argsForProbe += $mode }
        $process = Start-Process -FilePath "$scratch\probe.exe" -ArgumentList $argsForProbe -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -ne $expected) { throw "Fixture $mode expected $expected, got $($process.ExitCode)" }
    }
    Invoke-Fixture '/UPDATE' 0
    if (!(Test-Path -LiteralPath $key) -or !(Test-Path -LiteralPath $temp)) { throw 'Update removed pairing' }
    Invoke-Fixture '' 0
    if ((Test-Path -LiteralPath $key) -or (Test-Path -LiteralPath $temp)) { throw 'Uninstall left pairing' }
    Invoke-Fixture '' 0 # missing files / no hardware must be harmless
    [IO.File]::WriteAllText($key, 'synthetic-not-a-key')
    $lock = [IO.File]::Open($key, 'Open', 'Read', 'None')
    Invoke-Fixture '' 23
    if (!(Test-Path -LiteralPath $key)) { throw 'Locked fixture unexpectedly removed' }
    $log = Join-Path ([IO.Path]::GetTempPath()) "$fixtureId-esp32-uninstall-E23.log"
    $message = Get-Content -Raw -LiteralPath $log
    if (!$message.Contains($fixtureId) -or !$message.Contains('E23:') -or $message.Contains('synthetic-not-a-key')) { throw 'Missing or sensitive E23 guidance' }
    $lock.Dispose(); $lock = $null
    Invoke-Fixture '' 0
    $fixtureAcl = Get-Acl -LiteralPath $fixture
    $deniedAcl = Get-Acl -LiteralPath $fixture
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, [System.Security.AccessControl.FileSystemRights]::ListDirectory, [System.Security.AccessControl.AccessControlType]::Deny)
    $deniedAcl.AddAccessRule($rule)
    Set-Acl -LiteralPath $fixture -AclObject $deniedAcl
    try { Invoke-Fixture '' 23 }
    finally { Set-Acl -LiteralPath $fixture -AclObject $fixtureAcl; $fixtureAcl = $null }
    Invoke-Fixture '' 0
    foreach ($name in $preserved) {
        if ([IO.File]::ReadAllText((Join-Path $fixture $name)) -ne 'fixture-preserve') { throw "Changed $name" }
    }
    Write-Output 'PASS: compiled production NSIS hook; update preserves pairing, normal/repeated cleanup, locked-file and directory-ACL E23 exit/log, retry and unrelated preferences/Android preservation. Engineering fixture only; no real install/uninstall or hardware UAT.'
} finally {
    if ($lock) { $lock.Dispose() }
    if ($fixtureAcl) { Set-Acl -LiteralPath $fixture -AclObject $fixtureAcl }
    $fixtureLog = Join-Path ([IO.Path]::GetTempPath()) "$fixtureId-esp32-uninstall-E23.log"
    if (Test-Path -LiteralPath $fixtureLog) { Remove-Item -LiteralPath $fixtureLog }
    # Delete only this invocation's known UUID fixture folders, using one shell.
    foreach ($candidate in @($fixture, $scratch)) {
        $resolved = [IO.Path]::GetFullPath($candidate)
        if ((Split-Path $resolved -Leaf) -cne $fixtureId) { throw 'Unsafe fixture cleanup path' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
