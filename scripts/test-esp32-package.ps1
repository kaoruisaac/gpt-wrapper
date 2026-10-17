param([string]$Makensis = "$env:LOCALAPPDATA\tauri\NSIS\Bin\makensis.exe")
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$generated = Join-Path $repo 'src-tauri\target\release\nsis\x64'
$fixtureId = 'gptw-package-test-' + [guid]::NewGuid().ToString('N')
$scratch = Join-Path ([IO.Path]::GetTempPath()) $fixtureId
$configDir = Join-Path ([Environment]::GetFolderPath('ApplicationData')) $fixtureId
$installDir = Join-Path $scratch 'installed'
$product = "GPTW Package Fixture $fixtureId"
$registryPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$product"
New-Item -ItemType Directory -Path $scratch, $configDir | Out-Null
try {
    # Same rendered production installer and executable; only package identity,
    # output path and shortcuts are isolated. Never launch this real App binary:
    # its compiled runtime identifier remains the production identifier.
    $nsi = Get-Content -Raw -LiteralPath (Join-Path $generated 'installer.nsi')
    if (!$nsi.Contains('hooks.nsh') -or $nsi.Contains('DeleteAppDataCheckbox')) { throw 'Build the Phase 5 NSIS package first' }
    $nsi = $nsi.Replace('!define PRODUCTNAME "GPT Wrapper"', "!define PRODUCTNAME `"$product`"")
    $nsi = $nsi.Replace('!define BUNDLEID "cc.pedelec.gptwrapper"', "!define BUNDLEID `"$fixtureId`"")
    $nsi = $nsi.Replace('!define OUTFILE "nsis-output.exe"', "!define OUTFILE `"$scratch\package-probe.exe`"")
    $source = Join-Path $scratch 'installer.nsi'
    [IO.File]::WriteAllText($source, $nsi)
    Push-Location $generated
    try { & $Makensis /V2 /NOCD $source } finally { Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw 'Fixture package compile failed' }
    function Install-Fixture {
        $process = Start-Process -FilePath "$scratch\package-probe.exe" -ArgumentList @('/S', '/NS', "/D=$installDir") -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -ne 0 -or !(Test-Path -LiteralPath "$installDir\gpt-wrapper.exe") -or !(Test-Path -LiteralPath $registryPath)) { throw 'Full package install failed' }
    }
    Install-Fixture
    $key = Join-Path $configDir 'esp32-pairs.dpapi'
    [IO.File]::WriteAllText($key, 'synthetic-pairing')
    [IO.File]::WriteAllText((Join-Path $configDir '.esp32-fixture.tmp'), 'synthetic-pairing')
    foreach ($name in @('ble-controller.json','general-settings.json','project-settings.json','shortcut-settings.json')) {
        [IO.File]::WriteAllText((Join-Path $configDir $name), 'preserve')
    }
    # Direct /UPDATE tests the exact old-uninstaller path used for replacement.
    $process = Start-Process -FilePath "$installDir\uninstall.exe" -ArgumentList @('/S', '/UPDATE', "_?=$installDir") -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0 -or [IO.File]::ReadAllText($key) -ne 'synthetic-pairing') { throw 'Update uninstaller lost pairing' }
    Install-Fixture
    $hash = (Get-FileHash -LiteralPath "$installDir\gpt-wrapper.exe").Hash
    $expected = (Get-FileHash -LiteralPath (Join-Path $repo 'src-tauri\target\release\gpt-wrapper.exe')).Hash
    if ($hash -ne $expected) { throw 'Installed binary differs' }
    $process = Start-Process -FilePath "$installDir\uninstall.exe" -ArgumentList '/S' -WindowStyle Hidden -Wait -PassThru
    # NSIS self-copies the uninstaller to TEMP. Wait for its helper to finish.
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while ((Test-Path -LiteralPath "$installDir\uninstall.exe") -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if ((Test-Path -LiteralPath $installDir) -or (Test-Path -LiteralPath $registryPath) -or (Test-Path -LiteralPath $key) -or (Test-Path -LiteralPath (Join-Path $configDir '.esp32-fixture.tmp'))) { throw 'Full uninstall left package or pairing' }
    foreach ($name in @('ble-controller.json','general-settings.json','project-settings.json','shortcut-settings.json')) {
        if ([IO.File]::ReadAllText((Join-Path $configDir $name)) -ne 'preserve') { throw "Uninstall changed $name" }
    }
    Write-Output 'PASS: real complete NSIS install, /UPDATE uninstall + reinstall, installed binary SHA match, normal silent self-copy uninstall; registry/executable/pairing removed and preferences/Android preserved. Isolated package identity; no App launch, hardware, interactive upgrade page or User UAT.'
} finally {
    # Failure preserves installed package for diagnosis; never recursively delete
    # a live registered package or any real App profile.
    if (Test-Path -LiteralPath $registryPath) { Write-Warning "Fixture remains at $installDir; uninstall that fixture before retrying." }
    else {
        $fixtureRegistry = "HKCU:\Software\pedelec\$product"
        if ((Split-Path $fixtureRegistry -Leaf) -cne "GPTW Package Fixture $fixtureId") { throw 'Unsafe fixture registry cleanup' }
        # Non-recursive: refuse to delete any unexpected child registry keys.
        [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKey("Software\pedelec\$product", $false)
        foreach ($candidate in @($configDir, $scratch)) {
            $resolved = [IO.Path]::GetFullPath($candidate)
            if ((Split-Path $resolved -Leaf) -cne $fixtureId) { throw 'Unsafe fixture cleanup path' }
            Remove-Item -LiteralPath $resolved -Recurse -Force
        }
    }
}
