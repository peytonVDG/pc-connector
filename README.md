# PC Connector

Stream games from your powerful PC to another device on your home network. You get low latency, high quality video and audio, and your keyboard, mouse and controllers work on the remote PC.

The same app installs on both machines. Each one can be the **host** (shares its screen and receives input) or the **client** (shows the stream and sends input).

```
 Ubuntu gaming PC (host)                                   Surface Pro (client) ──HDMI──▶ projector
 ┌─────────────────────────────────────────┐   Wi-Fi/LAN   ┌──────────────────────────────────────┐
 │ screen capture (X11 / Wayland portal)   │   RTP / UDP   │ hardware decode (D3D11)              │
 │  → GPU convert/scale → NVENC/VA-API/QSV │ ────video───▶ │  → GPU YUV→RGB → present (mailbox)   │
 │ desktop audio → Opus (10 ms frames)     │ ────audio───▶ │ Opus → WASAPI low-latency output     │
 │ uinput: virtual keyboard, mouse,        │ ◀───input──── │ raw mouse, keyboard (incl. Win key), │
 │         Xbox 360 pads (with rumble)     │               │ Xbox/PlayStation/Switch controllers  │
 └─────────────────────────────────────────┘               └──────────────────────────────────────┘
```

## Quick start

### 1. On the Ubuntu PC (host)

```bash
git clone https://github.com/peytonvdg/pc-connector.git
cd pc-connector
./scripts/install-linux.sh
```

The script does the following:
* installs the GStreamer plugins
* builds the app (it installs Rust if needed)
* allows your desktop session to create a virtual keyboard, mouse and controllers through `/dev/uinput`
* adds **PC Connector** to your applications menu
* optionally starts hosting automatically when you log in

**Log out and back in once** so the input permission takes effect.

If you'd rather not build from source, the release tarball contains a prebuilt binary. Unpack it and run `scripts/install-linux.sh` from there.

Then open **PC Connector**, choose **Share this PC** and click **Start hosting**.

### 2. On the Surface Pro (client)

1. Download **PC-Connector-windows-x64.zip**. You can get it from the repository's *Releases* page, or from the latest *Build* run under the *Actions* tab. Unzip it anywhere, for example to your Desktop.
   It already includes GStreamer, so there's nothing else to install.
2. Run `pc-connector.exe`. When Windows Firewall asks, allow access on **Private networks**.
   (Or right-click `allow-firewall.cmd` → *Run as administrator* once.)
   Your Wi-Fi network must be set to **Private** under *Settings → Network & internet*.
3. Choose **Play on this device**. The Ubuntu PC shows up under *PCs on your network*. Click **Connect**.
4. **First connection only:** the Ubuntu PC shows a 4-digit PIN in the PC Connector window, as a desktop notification, or in the terminal. Type it on the Surface.
   On Wayland, the first connection also opens a "share your screen" dialog on the Ubuntu PC. Pick the monitor and click **Share**.
   Both are remembered, so after that you only need to click **Connect** from the couch.

The stream opens fullscreen on whichever screen the window is on. Plug the Surface into the projector, set Windows to show only on the projector (Win+P → *Second screen only*), and play.

### Shortcuts while streaming

| Keys | Action |
|---|---|
| **Ctrl+Alt+Shift+Z** | Release or capture the mouse and keyboard |
| **Ctrl+Alt+Shift+M** | Switch between game mouse (locked, raw movement) and desktop mouse (absolute pointer) |
| **Ctrl+Alt+Shift+F** | Toggle fullscreen |
| **Ctrl+Alt+Shift+S** | Show or hide the performance overlay |
| **Ctrl+Alt+Shift+Q** | Disconnect |

While input is captured, the Windows key, Alt+Tab, Alt+F4 and similar keys go to the Ubuntu PC. Controllers plugged into or paired with the Surface are forwarded automatically, as Xbox 360 controllers that every Linux game and Steam understands. Rumble works too.

## Getting the lowest latency

* **Use a cable for the gaming PC.** Connect it to the router with Ethernet, and keep the Surface on 5 GHz or 6 GHz Wi-Fi, ideally in the same room as the router or access point.
* Leave **V-Sync off** (the default). Frames are shown the moment they're decoded. If the projector shows tearing, turn on *V-Sync* in *Stream settings*. It adds at most one refresh interval.
* Keep **Wi-Fi low-latency mode** on (the default). It stops Windows' background Wi-Fi scans during the stream, which otherwise cause a stutter every minute or so on most laptops.
* Run games in **fullscreen** on the host and match the stream resolution to the projector (the *Match this display* default).
* Turn on the performance overlay (Ctrl+Alt+Shift+S) to see where the time goes: encode, network, decode and display. Packet loss shows in red.
* To test your network without a window, run `pc-connector.exe bench <host-ip>` from a terminal.

### What the app does to keep latency low

* **Hardware encoding** with low-latency presets: NVIDIA NVENC (P4 + ultra-low-latency tune), VA-API (AMD and Intel), Intel QSV, with x264 `zerolatency` as a fallback. There are no B-frames and no look-ahead, the rate control is CBR, and the VBV buffer holds about 2 frames, so each frame can be sent the moment it is encoded.
* **GPU colour conversion and scaling** before the encoder (CUDA, VA or D3D11), so the CPU doesn't touch the pixels.
* **No queueing.** A leaky one-frame queue in front of the encoder means that if anything falls behind, an old frame is dropped rather than everything getting later. The receive side keeps only the newest decoded frame.
* **No playback buffering for video.** Packets go straight from the network to the decoder. Packet loss triggers an immediate keyframe request instead of a long wait.
* **Hardware decoding** (D3D11/D3D12 on Windows; NVDEC/VA-API on Linux). The renderer draws as soon as a frame arrives, using mailbox/immediate presentation with a frame-latency of 1.
* **Wi-Fi priority tags.** Video is marked as Wi-Fi *video* traffic and audio/input as *voice* traffic (WMM/DSCP), so the router prioritises them.
* **Raw mouse input** sent over UDP the moment it happens, coalesced per event-loop pass. Key and button state is re-sent 10 times a second while held, so a lost packet can't leave a key stuck.
* **Opus audio** with 10 ms frames and a small (30 ms, adjustable) jitter buffer.

## Settings

Click **Stream settings** on the Play tab. Settings are saved automatically.

* **Resolution:** *Match this display* (default), *Host native*, or a fixed size. The host never upscales and always keeps the aspect ratio.
* **Frame rate:** 30 to 144 fps. Use 120 or 144 if both the PC and the projector can do it.
* **Bitrate:** *auto* (0) picks about 37 Mbps for 1080p60 H.264, or less for HEVC. Raise it for more detail; lower it if the overlay shows packet loss.
* **Codec:** *Automatic* uses HEVC when both machines have hardware support for it, and H.264 otherwise.

**Host settings** (Share tab):
* capture method
* encoder preference
* mute the PC's speakers while streaming
* paired devices
* audio bitrate
* keyframe safety interval

The settings file lives at `%APPDATA%\pc-connector\config\config.json` on Windows and `~/.config/pc-connector/config.json` on Linux. A log file `pc-connector.log` is written next to it.

## Command line

```
pc-connector                     # the app
pc-connector host                # open the app and start hosting immediately
pc-connector host --headless     # host without a window (autostart / SSH); prints pairing PINs
pc-connector connect <address>   # connect straight away
pc-connector bench <address>     # windowless connection test with live statistics
pc-connector probe               # what capture/encoders/decoders work on this machine
```

## Troubleshooting

* **The PC doesn't appear in the list.**
  * Check that hosting is started on the PC and that the Surface's network is *Private*.
  * Some routers block devices from seeing each other (guest networks, "AP/client isolation").
  * You can always type the PC's IP address, shown on the Share tab, under *Connect by address*.
* **Connects but the screen stays black (Ubuntu with Wayland).** Look at the PC's screen. It's waiting for you to approve screen sharing. If you denied it once, click *Forget screen-share permission* in Host settings.
* **Keyboard and mouse don't work.**
  * Run `pc-connector probe` on the PC and check that *input injection* says `true`.
  * If it doesn't, rerun `scripts/install-linux.sh` and log out and back in.
* **Choppy video or red packet loss in the overlay.**
  * Lower the bitrate, move closer to the access point, or use 5 GHz.
  * Plug the PC in with Ethernet.
* **Wrong encoder chosen, or the GPU isn't used.**
  * Run `pc-connector probe` on the PC.
  * NVIDIA needs the proprietary driver.
  * AMD and Intel need VA-API drivers (`mesa-va-drivers`, `intel-media-va-driver`). The install script installs both.
* **Windows SmartScreen warns about the app.** The app isn't code-signed. Click *More info → Run anyway*.

## Building from source

**Linux:** `./scripts/install-linux.sh` builds the app, or build it by hand:
```bash
sudo apt install build-essential pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libudev-dev
cargo build --release
```

**Windows:**
1. Install Rust (MSVC toolchain).
2. Run `packaging\windows\install-gstreamer.ps1`. It installs the GStreamer runtime and development files.
3. Run `cargo build --release`.
4. Run `packaging\windows\bundle.ps1` to make the portable folder in `dist\`.

Every push builds both platforms on GitHub Actions. Tags named `v*` publish a release.

## Ports

| Port | Protocol | Direction | Purpose |
|---|---|---|---|
| 48800 | TCP | client → host | control, pairing, stats |
| 48801 | UDP | broadcast | finding hosts on the LAN |
| 48802 | UDP | host → client | video (RTP) |
| 48803 | UDP | host → client | audio (RTP) |
| 48804 | UDP | client → host | input |

## Security

PC Connector is designed for your home network.

* A new device must be paired with a PIN shown on the host before it can see the screen or send input.
* Input packets are only accepted from the paired device's address, carrying that session's random token.
* Traffic isn't encrypted, so don't host on untrusted networks.
