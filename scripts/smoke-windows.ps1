[CmdletBinding(DefaultParameterSetName='Executable')]
param(
  [Parameter(Mandatory, ParameterSetName='Installer')]
  [string] $Installer,
  [Parameter(Mandatory, ParameterSetName='Executable')]
  [string] $Executable,
  [string] $Screenshot
)

$ErrorActionPreference = 'Stop'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('nexq-smoke-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $scratch
$previousWebViewArguments = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
$previousWebViewData = $env:WEBVIEW2_USER_DATA_FOLDER
$app = $null
try {
  if ($Installer) {
    $Installer = (Resolve-Path -LiteralPath $Installer).Path
    $installation = Join-Path $scratch 'app'
    $setup = Start-Process -FilePath $Installer -ArgumentList @('/S', "/D=$installation") -PassThru -Wait -WindowStyle Hidden
    if ($setup.ExitCode -ne 0) { throw "NSIS exited with code $($setup.ExitCode)." }
    $Executable = Join-Path $installation 'nexq-recorder.exe'
    if (!(Test-Path -LiteralPath (Join-Path $installation 'LICENSE.txt') -PathType Leaf)) {
      throw "The installer omitted LICENSE.txt."
    }
  }
  $Executable = (Resolve-Path -LiteralPath $Executable).Path
  $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
  $listener.Start()
  $port = $listener.LocalEndpoint.Port
  $listener.Stop()
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-address=127.0.0.1 --remote-debugging-port=$port"
  $env:WEBVIEW2_USER_DATA_FOLDER = Join-Path $scratch 'webview'
  $app = Start-Process -FilePath $Executable -PassThru -WindowStyle Hidden
  $smokeArguments = @((Join-Path $PSScriptRoot 'smoke-ui.mjs'), $port)
  if ($Screenshot) { $smokeArguments += $Screenshot }
  & node @smokeArguments
  if ($LASTEXITCODE -ne 0) { throw "NexQ Recorder UI smoke test failed." }
  $null = $app.CloseMainWindow()
  if (!$app.WaitForExit(8000)) { throw "NexQ Recorder did not close cleanly." }
  if ($app.ExitCode -ne 0) { throw "NexQ Recorder exited with code $($app.ExitCode)." }
} finally {
  if ($app -and !$app.HasExited) { $app.Kill(); $null = $app.WaitForExit(5000) }
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $previousWebViewArguments
  $env:WEBVIEW2_USER_DATA_FOLDER = $previousWebViewData
}
