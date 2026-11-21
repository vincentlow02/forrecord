[CmdletBinding(DefaultParameterSetName='Executable')]
param(
  [Parameter(Mandatory, ParameterSetName='Installer')]
  [string] $Installer,
  [Parameter(Mandatory, ParameterSetName='Executable')]
  [string] $Executable,
  [string] $Screenshot,
  [switch] $LibraryFixture,
  [Parameter(ParameterSetName='Installer')]
  [switch] $VerifyReinstall
)

$ErrorActionPreference = 'Stop'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('nexq-smoke-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $scratch
$previousWebViewArguments = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
$previousWebViewData = $env:WEBVIEW2_USER_DATA_FOLDER
$previousFixture = $env:NEXQ_SMOKE_FIXTURE
$installation = Join-Path $scratch 'app'
$uninstaller = Join-Path $installation 'uninstall.exe'
$fixtureTool = Join-Path $PSScriptRoot 'library-fixture.mjs'
$fixture = $null

function Install-Nexq {
  $setup = Start-Process -FilePath $Installer -ArgumentList @('/S', "/D=$installation") -PassThru -Wait -WindowStyle Hidden
  if ($setup.ExitCode -ne 0) { throw "NSIS exited with code $($setup.ExitCode)." }
  if (!(Test-Path -LiteralPath (Join-Path $installation 'LICENSE.txt') -PathType Leaf)) {
    throw "The installer omitted LICENSE.txt."
  }
  $programs = [Environment]::GetFolderPath('Programs')
  $links = @(Get-ChildItem -LiteralPath $programs -Recurse -Filter 'NexQ Recorder.lnk' -File)
  $shell = New-Object -ComObject WScript.Shell
  $installedExe = Join-Path $installation 'nexq-recorder.exe'
  $matchingLinks = @($links | Where-Object { $shell.CreateShortcut($_.FullName).TargetPath -eq $installedExe })
  if ($matchingLinks.Count -eq 0) { throw "The installer did not create a current-user Start Menu shortcut." }
}

function Uninstall-Nexq {
  $uninstall = Start-Process -FilePath $uninstaller -ArgumentList '/S' -Wait -WindowStyle Hidden -PassThru
  if ($uninstall.ExitCode -ne 0 -or (Test-Path -LiteralPath $Executable -PathType Leaf)) {
    throw "The uninstall did not remove the installed application."
  }
}

function Invoke-WebViewSmoke {
  $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
  $listener.Start()
  $port = $listener.LocalEndpoint.Port
  $listener.Stop()
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-address=127.0.0.1 --remote-debugging-port=$port"
  $env:WEBVIEW2_USER_DATA_FOLDER = Join-Path $scratch "webview-$port"
  $stdout = Join-Path $scratch 'app.stdout.log'
  $stderr = Join-Path $scratch 'app.stderr.log'
  $app = Start-Process -FilePath $Executable -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdout -RedirectStandardError $stderr
  try {
    $smokeArguments = @((Join-Path $PSScriptRoot 'smoke-ui.mjs'), $port)
    if ($Screenshot) { $smokeArguments += $Screenshot }
    & node @smokeArguments
    if ($LASTEXITCODE -ne 0) {
      $runtimePaths = @(
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
        'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
      )
      $versions = Get-ItemPropertyValue -LiteralPath $runtimePaths -Name pv -ErrorAction SilentlyContinue
      Write-Host "WebView2 Runtime $($versions -join ', '). App exited $($app.HasExited)."
      if ($app.HasExited) { Write-Host "App exit code $($app.ExitCode)." }
      Get-Content -LiteralPath $stdout,$stderr -Tail 20
      throw "NexQ Recorder UI smoke test failed."
    }
    $null = $app.CloseMainWindow()
    if (!$app.WaitForExit(8000)) { throw "NexQ Recorder did not close cleanly." }
    if ($app.ExitCode -ne 0) { throw "NexQ Recorder exited with code $($app.ExitCode)." }
  } finally {
    if (!$app.HasExited) { $app.Kill(); $null = $app.WaitForExit(5000) }
  }
}

try {
  if ($Installer) {
    if (Test-Path -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\NexQ Recorder') {
      throw 'Run installer tests under a Windows account without an existing NexQ installation.'
    }
    $Installer = (Resolve-Path -LiteralPath $Installer).Path
    $Executable = Join-Path $installation 'nexq-recorder.exe'
    Install-Nexq
  }
  $Executable = (Resolve-Path -LiteralPath $Executable).Path
  if ($LibraryFixture -or $VerifyReinstall) {
    $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
    $appData = Join-Path $localAppData 'com.nexq.recorder'
    $fixtureRoot = Join-Path $appData 'Recordings'
    $config = Join-Path $appData 'settings.json'
    if (Test-Path -LiteralPath $config -PathType Leaf) {
      $folder = (Get-Content -LiteralPath $config -Raw | ConvertFrom-Json).folder
      if ($folder -and $folder.Replace('\\?\', '') -ne $fixtureRoot) {
        throw 'Run library fixture tests with the default NexQ recordings folder.'
      }
    }
    $fixture = [Guid]::NewGuid().ToString('N')
    & node $fixtureTool create $fixtureRoot $fixture
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the synthetic legacy library fixture.' }
    $env:NEXQ_SMOKE_FIXTURE = $fixture
  }
  Invoke-WebViewSmoke
  if ($VerifyReinstall) {
    Uninstall-Nexq
    $manifest = Join-Path $fixtureRoot "$fixture/session.json"
    if (!(Test-Path -LiteralPath $manifest -PathType Leaf)) { throw 'Uninstall removed the user recording.' }
    Install-Nexq
    Invoke-WebViewSmoke
    Write-Host "Start Menu, uninstall, reinstall, and retained recording playback passed."
  }
} finally {
  if ($Installer -and (Test-Path -LiteralPath $uninstaller -PathType Leaf)) { Uninstall-Nexq }
  if ($fixture) {
    & node $fixtureTool remove $fixtureRoot $fixture
    if ($LASTEXITCODE -ne 0) { throw 'The synthetic fixture could not be removed.' }
  }
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $previousWebViewArguments
  $env:WEBVIEW2_USER_DATA_FOLDER = $previousWebViewData
  $env:NEXQ_SMOKE_FIXTURE = $previousFixture
}
