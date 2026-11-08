# Installs the GStreamer MSVC runtime + development files (used by CI and for local
# Windows builds). Exports GSTREAMER_1_0_ROOT_MSVC_X86_64 / PKG_CONFIG_PATH / PATH for
# later GitHub Actions steps when run there.
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$versions = @("1.26.8", "1.26.7", "1.26.6", "1.26.5", "1.26.4", "1.26.3", "1.26.2", "1.24.13", "1.24.12")
$tmp = Join-Path $env:TEMP "gst-install"
New-Item -ItemType Directory -Force $tmp | Out-Null

function Try-Download($url, $dest) {
    try {
        Invoke-WebRequest -Uri $url -OutFile $dest -UseBasicParsing
        return $true
    } catch {
        return $false
    }
}

function Find-GstRoot {
    $candidates = @(
        [Environment]::GetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", "Machine"),
        [Environment]::GetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", "User"),
        "C:\Program Files\gstreamer\1.0\msvc_x86_64",
        "C:\gstreamer\1.0\msvc_x86_64"
    )
    foreach ($c in $candidates) {
        if ($c -and (Test-Path (Join-Path $c "lib\pkgconfig\gstreamer-1.0.pc"))) { return $c.TrimEnd('\') }
    }
    return $null
}

$installed = $false
foreach ($v in $versions) {
    $base = "https://gstreamer.freedesktop.org/data/pkg/windows/$v/msvc"
    $rt = Join-Path $tmp "runtime-$v.msi"
    $dev = Join-Path $tmp "devel-$v.msi"
    if ((Try-Download "$base/gstreamer-1.0-msvc-x86_64-$v.msi" $rt) -and
        (Try-Download "$base/gstreamer-1.0-devel-msvc-x86_64-$v.msi" $dev)) {
        Write-Host "Installing GStreamer $v (MSI)"
        foreach ($msi in @($rt, $dev)) {
            $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList "/i `"$msi`" /qn /norestart INSTALLLEVEL=1000 ADDLOCAL=ALL"
            if ($p.ExitCode -ne 0) { throw "msiexec failed for $msi with exit code $($p.ExitCode)" }
        }
        $installed = $true
        break
    }
    $exe = Join-Path $tmp "gstreamer-$v.exe"
    if (Try-Download "$base/gstreamer-1.0-msvc-x86_64-$v.exe" $exe) {
        Write-Host "Installing GStreamer $v (installer)"
        $p = Start-Process $exe -Wait -PassThru -ArgumentList "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /ALLUSERS /TYPE=full"
        if ($p.ExitCode -ne 0) { throw "installer failed with exit code $($p.ExitCode)" }
        $installed = $true
        break
    }
    Write-Host "GStreamer $v not available, trying an older version"
}

$root = Find-GstRoot
if (-not $root) {
    Write-Host "Falling back to Chocolatey packages"
    choco install -y --no-progress gstreamer gstreamer-devel
    $root = Find-GstRoot
}
if (-not $root) { throw "GStreamer installation not found" }
Write-Host "GStreamer root: $root"

$pkgconfig = Join-Path $root "bin\pkg-config.exe"
if (-not (Test-Path $pkgconfig) -and -not (Get-Command pkg-config -ErrorAction SilentlyContinue)) {
    choco install -y --no-progress pkgconfiglite
}

$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = $root
$env:PKG_CONFIG_PATH = Join-Path $root "lib\pkgconfig"
$env:PATH = (Join-Path $root "bin") + ";" + $env:PATH
if ($env:GITHUB_ENV) {
    "GSTREAMER_1_0_ROOT_MSVC_X86_64=$root" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    "PKG_CONFIG_PATH=$(Join-Path $root 'lib\pkgconfig')" | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8
    (Join-Path $root "bin") | Out-File -FilePath $env:GITHUB_PATH -Append -Encoding utf8
}
& pkg-config --modversion gstreamer-1.0
