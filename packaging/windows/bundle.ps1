# Builds the portable Windows folder: pc-connector.exe + the GStreamer runtime it needs,
# so nothing else has to be installed on the Surface / laptop.
param(
    [string]$GstRoot = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64,
    [string]$Exe = "target\release\pc-connector.exe",
    [string]$Out = "dist\PC-Connector-windows-x64"
)
$ErrorActionPreference = "Stop"

if (-not $GstRoot) {
    foreach ($c in @("C:\Program Files\gstreamer\1.0\msvc_x86_64", "C:\gstreamer\1.0\msvc_x86_64")) {
        if (Test-Path $c) { $GstRoot = $c; break }
    }
}
if (-not $GstRoot) { throw "GStreamer not found; set GSTREAMER_1_0_ROOT_MSVC_X86_64" }
$GstRoot = $GstRoot.TrimEnd('\')
Write-Host "Bundling GStreamer from $GstRoot"

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
$pluginDir = Join-Path $Out "gstreamer\lib\gstreamer-1.0"
$scannerDir = Join-Path $Out "gstreamer\libexec\gstreamer-1.0"
New-Item -ItemType Directory -Force $pluginDir | Out-Null
New-Item -ItemType Directory -Force $scannerDir | Out-Null

Copy-Item $Exe $Out
# Core libraries and every dependency of the plugins (codecs, glib, ffmpeg, ...).
Copy-Item (Join-Path $GstRoot "bin\*.dll") $Out

$plugins = @(
    # pipeline basics
    "gstcoreelements", "gstapp", "gstvideotestsrc", "gstvideoconvertscale", "gstvideoconvert", "gstvideoscale",
    "gstaudioconvert", "gstaudioresample", "gstautodetect",
    # network / RTP
    "gstudp", "gstrtp", "gstrtpmanager",
    # parsers and codecs
    "gstvideoparsersbad", "gstopus", "gstlibav", "gstx264", "gstx265", "gstopenh264",
    # Windows GPU: capture, convert, hardware encode/decode
    "gstd3d11", "gstd3d12", "gstnvcodec", "gstqsv", "gstamfcodec", "gstmediafoundation",
    # audio devices
    "gstwasapi", "gstwasapi2", "gstdirectsound"
)
foreach ($p in $plugins) {
    $f = Join-Path $GstRoot "lib\gstreamer-1.0\$p.dll"
    if (Test-Path $f) { Copy-Item $f $pluginDir } else { Write-Host "  (plugin $p not in this GStreamer build, skipped)" }
}
$scanner = Join-Path $GstRoot "libexec\gstreamer-1.0\gst-plugin-scanner.exe"
if (Test-Path $scanner) { Copy-Item $scanner $scannerDir }

Copy-Item "packaging\windows\allow-firewall.cmd" $Out
Copy-Item "README.md" $Out

$size = (Get-ChildItem -Recurse $Out | Measure-Object -Property Length -Sum).Sum / 1MB
Write-Host ("Bundle ready: {0} ({1:N0} MB)" -f $Out, $size)

# Smoke test: the bundled exe must start and find its plugins.
# (GUI-subsystem exe: Start-Process -Wait is needed to get its exit code.)
$proc = Start-Process -FilePath (Join-Path $Out "pc-connector.exe") -ArgumentList "probe" -Wait -PassThru -NoNewWindow
if ($proc.ExitCode -ne 0) { throw "bundled pc-connector.exe probe failed ($($proc.ExitCode))" }
