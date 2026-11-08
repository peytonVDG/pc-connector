#!/usr/bin/env bash
# Installs PC Connector on Ubuntu / Debian (22.04 or newer).
#
#  * installs the GStreamer plugins used for capture, hardware encoding and audio
#  * builds the app from source if no prebuilt binary sits next to this script
#  * lets your desktop session create virtual keyboards/mice/gamepads (/dev/uinput)
#  * adds a menu entry (and optionally starts hosting automatically at login)
#
# Usage:  ./scripts/install-linux.sh            (from the source tree or the release tarball)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo"; fi

say() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }

say "Installing GStreamer runtime packages"
$SUDO apt-get update
$SUDO apt-get install -y \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
  gstreamer1.0-x gstreamer1.0-pipewire gstreamer1.0-pulseaudio \
  pulseaudio-utils libnotify-bin
# Optional extras: legacy VA-API encoder (older Ubuntu releases) and VA drivers.
$SUDO apt-get install -y gstreamer1.0-vaapi 2>/dev/null || true
$SUDO apt-get install -y mesa-va-drivers 2>/dev/null || true
$SUDO apt-get install -y intel-media-va-driver 2>/dev/null || true

BIN=""
for candidate in "$ROOT/pc-connector" "$HERE/pc-connector" "$ROOT/target/release/pc-connector"; do
  if [ -x "$candidate" ] && [ ! -d "$candidate" ]; then BIN="$candidate"; break; fi
done

if [ -z "$BIN" ] && [ -f "$ROOT/Cargo.toml" ]; then
  say "Building from source"
  $SUDO apt-get install -y build-essential pkg-config curl \
    libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libudev-dev
  if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
      export PATH="$HOME/.cargo/bin:$PATH"
    else
      say "Installing Rust (rustup)"
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      export PATH="$HOME/.cargo/bin:$PATH"
    fi
  fi
  (cd "$ROOT" && cargo build --release)
  BIN="$ROOT/target/release/pc-connector"
fi

if [ -z "$BIN" ]; then
  echo "Could not find a pc-connector binary or the source code next to this script." >&2
  exit 1
fi

say "Installing $BIN to /usr/local/bin/pc-connector"
$SUDO install -m 0755 "$BIN" /usr/local/bin/pc-connector

say "Allowing your desktop session to create virtual input devices (uinput)"
$SUDO tee /etc/udev/rules.d/60-pc-connector-uinput.rules >/dev/null <<'EOF'
# PC Connector: let the logged-in desktop user create virtual keyboards, mice and gamepads.
KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"
EOF
echo uinput | $SUDO tee /etc/modules-load.d/pc-connector-uinput.conf >/dev/null
$SUDO modprobe uinput || true
$SUDO udevadm control --reload-rules || true
$SUDO udevadm trigger --name-match=uinput || true

say "Adding PC Connector to the applications menu"
mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/pc-connector.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=PC Connector
Comment=Stream games from this PC to your other devices
Exec=/usr/local/bin/pc-connector
Icon=video-display
Terminal=false
Categories=Game;Network;
EOF

if command -v ufw >/dev/null 2>&1 && $SUDO ufw status 2>/dev/null | grep -q "Status: active"; then
  say "Opening firewall ports 48800-48804 (ufw)"
  $SUDO ufw allow 48800:48804/tcp
  $SUDO ufw allow 48800:48804/udp
fi

read -r -p $'\nStart hosting automatically when you log in? [y/N] ' answer || answer=""
if [[ "$answer" =~ ^[Yy] ]]; then
  mkdir -p "$HOME/.config/autostart"
  cat > "$HOME/.config/autostart/pc-connector-host.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=PC Connector (host)
Exec=/usr/local/bin/pc-connector host
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
  echo "Autostart enabled (remove ~/.config/autostart/pc-connector-host.desktop to undo)."
fi

say "Checking what works on this PC"
/usr/local/bin/pc-connector probe || true

cat <<'EOF'

Done! Start "PC Connector" from the applications menu and click "Share this PC".
If keyboard/mouse input does not work yet, log out and back in once (for the uinput permission).
EOF
