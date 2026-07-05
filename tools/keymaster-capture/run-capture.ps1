# Keymaster capture harness.
#
# Boots FXServer 31623 with your Pebble license key, routing its HTTPS through
# a local mitmproxy that logs only the CFX platform handshake (license ->
# tokens, nucleus register, server-list ingress, policy). FXServer builds its
# requests with CURLOPT_SSL_VERIFYPEER=0, so no CA trust setup is needed.
#
# USAGE:
#   1. Edit capture.cfg and paste your real cfxk_... key into sv_licenseKey.
#   2. From this folder:  powershell -ExecutionPolicy Bypass -File .\run-capture.ps1
#   3. Let it run ~30-60s until the console shows the ingress heartbeat, then
#      press Ctrl+C. Inspect cfx-capture.jsonl.
#
# The license key travels in the portal-api request body/headers and WILL be
# in cfx-capture.jsonl — review/redact before sharing that file.

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$fxserver = 'D:\Dev\Fivem\Servers\WTF\Artifacts\windows\31623\FXServer.exe'
$mitmdump = Join-Path $env:APPDATA 'Python\Python313\Scripts\mitmdump.exe'
$outFile = Join-Path $here 'cfx-capture.jsonl'
$proxyPort = 8080

if (-not (Test-Path $fxserver)) { throw "FXServer not found: $fxserver" }
if (-not (Test-Path $mitmdump)) { throw "mitmdump not found: $mitmdump (pip install mitmproxy)" }
if ((Get-Content (Join-Path $here 'capture.cfg') -Raw) -match 'cfxk_REPLACE_ME') {
    throw "Paste your real license key into capture.cfg (sv_licenseKey) first."
}

# Fresh capture file each run.
if (Test-Path $outFile) { Remove-Item $outFile }

Write-Host "[*] Starting mitmdump on 127.0.0.1:$proxyPort ..." -ForegroundColor Cyan
$env:CFX_CAPTURE_OUT = $outFile
$mitm = Start-Process -FilePath $mitmdump -PassThru -NoNewWindow -ArgumentList @(
    '--listen-host', '127.0.0.1',
    '--listen-port', "$proxyPort",
    '-s', (Join-Path $here 'cfx_capture.py'),
    '--set', 'flow_detail=0',
    '-q'
)
Start-Sleep -Seconds 2

# libcurl honours these env vars; set every casing variant to be safe.
$proxy = "http://127.0.0.1:$proxyPort"
$env:HTTPS_PROXY = $proxy; $env:https_proxy = $proxy
$env:HTTP_PROXY  = $proxy; $env:http_proxy  = $proxy
$env:ALL_PROXY   = $proxy; $env:all_proxy   = $proxy

Write-Host "[*] Launching FXServer (Ctrl+C to stop) ..." -ForegroundColor Cyan
Write-Host "    Capturing to $outFile" -ForegroundColor DarkGray
try {
    Push-Location 'D:\Dev\Fivem\Servers\WTF\baston'
    & $fxserver +exec (Join-Path $here 'capture.cfg')
}
finally {
    Pop-Location
    Write-Host "[*] Stopping mitmdump ..." -ForegroundColor Cyan
    if ($mitm -and -not $mitm.HasExited) { Stop-Process -Id $mitm.Id -Force }
    if (Test-Path $outFile) {
        $n = (Get-Content $outFile | Measure-Object -Line).Lines
        Write-Host "[+] Captured $n CFX request(s) -> $outFile" -ForegroundColor Green
    } else {
        Write-Host "[!] No CFX traffic captured. If FXServer ignored the proxy env," -ForegroundColor Yellow
        Write-Host "    set the Windows system proxy to $proxy and retry." -ForegroundColor Yellow
    }
}
