# Linker on Ubuntu — Runbook

Ubuntu is the **KVM server** (it owns the physical keyboard/mouse), the
**camera sink** (the Mac's camera appears here as `/dev/video10`,
"Linker Camera") and the **audio sink**: the Mac's microphone appears as the
input device **"Linker Microphone"**, and anything an app plays to the output
device **"Linker Speaker"** comes out of the Mac's speakers/headphones.
Tested target: Ubuntu 24.04 LTS, GNOME, Wayland or X11 (camera/KVM also 22.04;
audio needs PipeWire ≥ 0.3.49, the default sound server since Ubuntu 22.10).

> Do the macOS side too ([macos.md](macos.md)). Pairing needs both machines.

---

## 1. Prerequisites

| Need | Why |
|---|---|
| Ubuntu with GNOME, on the **same Wi-Fi/LAN** as the Mac | mDNS discovery + direct link |
| `sudo` rights (once) | kernel modules, udev rule, packages |
| Rust toolchain (stable) | build `linkerd` |
| Network allowing device-to-device traffic | many "guest" Wi-Fi networks block it (see Troubleshooting) |
| PipeWire as the sound server (Ubuntu ≥ 22.10 default) | audio: `pactl info` must say `Server Name: PulseAudio (on PipeWire …)` |

Install Rust if needed:

```bash
curl https://sh.rustup.rs -sSf | sh -s -- -y
source "$HOME/.cargo/env"
```

## 2. Build, install, start at boot (one command)

From the repository root:

```bash
./runbook/linux/install.sh
```

What it does (all idempotent — rerun after `git pull` to upgrade):

1. `apt install build-essential pkg-config cmake libpipewire-0.3-dev libspa-0.2-dev libclang-dev pulseaudio-utils v4l2loopback-dkms v4l-utils libsecret-tools linux-headers-$(uname -r)`
   (cmake builds the bundled Opus codec; the PipeWire/SPA headers and libclang
   are for the PipeWire bindings; `pulseaudio-utils` gives `pactl` for checks)
2. Loads **uinput** and **v4l2loopback** now and at every boot:
   - `/etc/modules-load.d/linker.conf` → `uinput`, `v4l2loopback`
   - `/etc/modprobe.d/linker-v4l2loopback.conf` →
     `options v4l2loopback devices=1 video_nr=10 card_label="Linker Camera" exclusive_caps=1`
3. Device permissions: udev rule `/etc/udev/rules.d/70-linker-uinput.rules`
   (`/dev/uinput` → group `input`, mode 0660) and adds you to groups **input**
   (read `/dev/input/event*`, write `/dev/uinput`) and **video** (`/dev/video10`).
4. If `ufw` is active: allows UDP 47800 (QUIC), 47801 (discovery), 5353 (mDNS).
5. `cargo build --release`, installs `~/.local/bin/linkerd`, writes a commented
   `~/.config/linker/config.toml`.
6. Installs and enables the **systemd user service**
   `~/.config/systemd/user/linker.service` (`systemctl --user enable --now`)
   and runs `loginctl enable-linger $USER` so it runs from boot, even before login.

**Then log out and back in (or reboot)** so the new `input`/`video` group
membership takes effect.

<details><summary>Manual equivalent (if you prefer to run each step yourself)</summary>

```bash
sudo apt install -y build-essential pkg-config cmake libpipewire-0.3-dev libspa-0.2-dev libclang-dev pulseaudio-utils v4l2loopback-dkms v4l-utils libsecret-tools linux-headers-$(uname -r)
echo -e "uinput\nv4l2loopback" | sudo tee /etc/modules-load.d/linker.conf
echo 'options v4l2loopback devices=1 video_nr=10 card_label="Linker Camera" exclusive_caps=1' | sudo tee /etc/modprobe.d/linker-v4l2loopback.conf
sudo modprobe uinput
sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Linker Camera" exclusive_caps=1
echo 'KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/70-linker-uinput.rules
sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=uinput
sudo usermod -aG input,video "$USER"
cargo build --release -p linker-daemon
install -D -m755 target/release/linkerd ~/.local/bin/linkerd
install -D -m644 packaging/linux/linker.service ~/.config/systemd/user/linker.service
systemctl --user daemon-reload && systemctl --user enable --now linker.service
loginctl enable-linger "$USER"
```
</details>

Service control:

```bash
systemctl --user status linker      # running?
journalctl --user -u linker -f      # live logs
systemctl --user restart linker     # after editing config.toml
```

## 3. Check what works on this machine

```bash
linkerd check
```

Every line should be `[ OK ]`. Typical output on a fully set-up machine:

```
  [ OK ] clipboard watch        started
  [ OK ] notification           test banner sent — did it appear?
  [ OK ] dnd read               currently off
  [ OK ] dnd watch              available
  [ OK ] sleep/wake events      available
  [ OK ] kvm capture (server)   screen (1920.0, 1080.0)
  [ OK ] camera sink            /dev/video10 via openh264 (software)
  [ OK ] audio virtual speaker  "Linker Speaker (check)" receives app audio — 48000 Hz, period 128 frames (2.67 ms)
  [ OK ] audio virtual mic      "Linker Microphone (check)" delivers to recording apps — 48000 Hz, period 128 frames (2.67 ms)
  [ OK ] audio loop (local)     speaker → Opus → jitter buffer → mic: latency 11.8 ms | 4.0s recorded, gaps 0, underruns 0, PLC 0 of 1098 frames | buffer 6 ms
  [ OK ] daemon                 running (/run/user/1000/linker.sock)
  [ OK ] audio live             audio mic     : recv playing | buf   7ms (target   6) | … | latency 21 ms
```

The audio probe creates temporary "(check)" copies of the two virtual
devices (the daemon's own devices are untouched), plays a tone into the
speaker like an app would, sends it through the real audio engine (Opus,
redundancy, reorder, adaptive jitter buffer) and records it back from the
microphone like Zoom would — reporting sample rate, callback period,
measured latency and any loss/gaps. `audio live` lines appear while the
daemon is streaming (network loss and latency over the real link).

A `[FAIL]` line says exactly what is missing (group membership, module…).

## 4. Pair with the Mac (first time only)

On **both** machines, within 2 minutes of each other:

```bash
linkerd pair
```

Each terminal prints a 6-digit code, e.g. `471 524`. **Compare the two
screens.** If they match, answer `y` on both. If they differ, answer `n` —
something else on the network answered; try again.

```
  Found "MacBook". Pairing code:
        471 524
  Does the other machine show the SAME code? [y/N] y
✔ Paired with "MacBook". The link connects automatically from now on.
```

From now on the machines find each other and connect by themselves (also
after reboots, sleep, Wi-Fi changes). Only this paired Mac can connect.
`linkerd unpair` forgets it.

## 5. Using each feature

| Feature | How to use | Settings (`~/.config/linker/config.toml`) |
|---|---|---|
| **Clipboard** | Copy on one machine, paste on the other. Text, rich text, images and files. Received files land in `~/Downloads/Linker/clip-…/` and are put on the clipboard as files. iPhone → Mac → Ubuntu works through Apple's Universal Clipboard on the Mac. | `[clipboard]` |
| **Notifications** | Mac notifications appear here as normal GNOME banners ("App — Title"). | `[notifications] show`, `when_dnd`, `ignore_apps` |
| **Do Not Disturb** | Toggle DND in the GNOME quick settings; the Mac follows (and vice versa). | `[dnd]` |
| **KVM** | Push the mouse past the **right** edge of your screen → the pointer appears on the Mac; your keyboard types there. Move back past the Mac's left edge to return. **Ctrl+Alt+Shift+Esc** releases control immediately. | `[kvm] peer_edge = "right"` (left/top/bottom), `panic_hotkey`, `screen_width/height` |
| **Camera** | In any app (Firefox, Chrome, Zoom, OBS, Cheese…) pick **"Linker Camera"**. The Mac camera turns on while an app uses it and off ~5 s after. | `[camera] width/height/fps/bitrate_kbps`, `mode = "on_demand" \| "always"` |
| **Microphone** (Mac → Ubuntu) | In the app pick the microphone **"Linker Microphone"**. The Mac's microphone (orange dot in the Mac menu bar) turns on while an app records from it and off ~5 s after. | `[audio] mic`, `mic_mode = "on_demand" \| "always"`, `mic_name`; codec/buffer: `bitrate_kbps`, `frame_ms`, `redundancy`, `min/max_buffer_ms` (this machine plays the mic stream, so its settings apply) |
| **Speaker** (Ubuntu → Mac) | In the app pick the speaker/output **"Linker Speaker"**. That app's audio plays on the Mac; everything else on Ubuntu keeps using your normal output (no echo, no double audio). | `[audio] speaker`, `speaker_name` (codec settings of this stream are in the **Mac's** config) |

### Zoom (camera + microphone + speaker from the Mac)

1. Zoom → **Settings → Video → Camera**: **Linker Camera**.
2. Zoom → **Settings → Audio → Microphone**: **Linker Microphone**;
   **Speaker**: **Linker Speaker**. Turn *Automatically adjust microphone
   volume* off if the level pumps; the Mac's own mic level is set on the Mac.
3. Optional: *Original sound for musicians* / *Suppress background noise: Low*
   — the stream is already full-band 48 kHz.

Browsers (Meet, Teams, Jitsi): pick the same three devices in the site's
settings (Chrome/Firefox list them like USB devices). Any PulseAudio/PipeWire
app works the same way; `pavucontrol` can move a running app to "Linker Speaker".
Use headphones on the Mac in calls, or the Mac's speakers will feed the Mac's
microphone (like any speakerphone).

Ctrl on this keyboard acts as ⌘ on the Mac (so Ctrl+C / Ctrl+V work as
expected). Disable with `[kvm] swap_ctrl_cmd = false` in the **Mac's** config.

## 6. Verify it works

```bash
linkerd status
```

```
this device : ubuntu-desktop  [4df7b0bfcb17]
paired with : MacBook  [8f90b5339349]
link        : Connected
peer        : MacBook at 192.168.1.23:47800  rtt 2.1 ms  up 312s
features:
  audio          sink: Linker Microphone ready, Linker Speaker ready
  camera         sink: /dev/video10 ready (waiting for an app / the Mac)
  clipboard      active
  dnd            active (bidirectional)
  kvm            server: local (screen 1920x1080, peer on Right edge)
  notifications  show-only (capture: …)
  power          active (OS sleep/wake events)
audio mic     : recv playing | buf   7ms (target   6) | gap   6ms | recovered 0 PLC 0 late 0 | underruns 0 trims 0 | speed +0.0% | loss 0.0% | 256 kbps 5 ms x3 | latency 21 ms | cb 2.8ms | Linker Microphone
audio speaker : send streaming | 200 pkt/s 810 kbps | peak -18.5 dBFS | peer buf 7ms (target 6) PLC 0 underruns 0 | 256 kbps 5 ms x3 | latency 22 ms | Linker Speaker
```

Audio lines (modelled on audiobridge's stats line), per stream:

| Field | Meaning |
|---|---|
| `recv`/`send` | this machine plays (decodes) / captures (encodes) the stream |
| `buf … (target …)` | adaptive jitter buffer level / the target it chose (= added latency). The target is the largest packet gap seen in the last 30 s, within `min_buffer_ms`..`max_buffer_ms` |
| `gap` | largest pause between packets in the last 30 s (normal: ≤ 10–25 ms on good Wi-Fi) |
| `recovered` | lost/late packets rebuilt from the copies carried by later packets (inaudible) |
| `PLC` | frames that never arrived; Opus concealed them (no silence, no click) |
| `late` | packets that arrived after their audio was played (harmless) |
| `underruns` | the buffer ran dry (audible); 1–2 right after start = adaptation, should then stay constant |
| `trims` | a burst > 250 ms was cut with a crossfade |
| `speed` | playback speed tweak (≤ ±0.5 %) absorbing clock drift between the machines |
| `loss` | datagrams lost on the network in the last second (before redundancy) |
| `… kbps 5 ms x3` | Opus bitrate (automatic: 128 → 256 on a clean link), frame length, copies per packet |
| `latency` | capture → playout (network part measured with the clock offset from the heartbeats) |
| `cb` | largest gap between audio callbacks (PipeWire / CoreAudio) |
| `peak` (send) | loudest captured sample — `-inf dBFS` = digital silence |

Feature-by-feature:

- **Link**: `link : Connected`. Reboot either machine — it reconnects within
  ~15 s of both being up. Close the Mac lid for a minute, reopen: reconnects in
  a few seconds. Turn the Mac off: after 60 s you get **one** "peer not found"
  banner, then silence until it's back.
- **Camera**: `v4l2-ctl --list-devices` shows `Linker Camera (platform:v4l2loopback-000): /dev/video10`.
  Open <https://webcamtests.com> (or `ffplay /dev/video10`) and choose
  "Linker Camera": the Mac's camera light turns on and the live picture
  appears. `linkerd status` then shows fps, latency and loss.
- **Audio devices**: `pactl list short sources | grep linker` shows
  `linker_microphone`, `pactl list short sinks | grep linker` shows
  `linker_speaker` — that's exactly what Zoom/Chrome enumerate.
- **Microphone**: `pw-record --target linker_microphone /tmp/mic.wav` (talk on
  the Mac, Ctrl+C, then `pw-play /tmp/mic.wav`). The Mac's orange mic dot is
  on while recording. Or Zoom → Settings → Audio → *Test Mic*.
- **Speaker**: `pw-play --target linker_speaker /usr/share/sounds/alsa/Front_Center.wav`
  → heard on the Mac. Or Zoom → *Test Speaker* with "Linker Speaker" selected.
- **Clipboard**: `echo hello | xclip -sel clip` (or copy in any app) → ⌘V on
  the Mac. Copy a screenshot on the Mac (⌘⇧⌃4) → paste into GIMP/LibreOffice here.
- **Notifications**: send yourself an iMessage / Mail on the Mac → banner here.
- **DND**: toggle in GNOME → Mac Focus switches within ~1 s.
- **KVM**: as in §5; type in a Mac text field.
- **Encryption**: `sudo tcpdump -i any -X udp port 47800` shows only QUIC
  (TLS 1.3) ciphertext — no readable clipboard text.

## 7. Troubleshooting

| Symptom | Fix |
|---|---|
| `camera sink … no v4l2loopback device labelled "Linker Camera"` | `sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Linker Camera" exclusive_caps=1`, then `systemctl --user restart linker`. If modprobe says *Key was rejected by service* / *Operation not permitted*: **Secure Boot** blocks the unsigned DKMS module — `sudo mokutil --import /var/lib/shim-signed/mok/MOK.der`, reboot, choose *Enroll MOK* in the blue screen. If `/dev/video10` is taken, change `video_nr`. |
| Module not loaded after reboot | `cat /etc/modules-load.d/linker.conf` must list `v4l2loopback`; after a kernel update run `sudo apt install linux-headers-$(uname -r)` so DKMS rebuilds it. |
| `cannot read /dev/input/event*` / `cannot open /dev/uinput` (Permission denied) | `groups` must include `input` and `video` — log out/in after install. Check `ls -l /dev/uinput` → `crw-rw---- root input`. If not: `sudo udevadm trigger --name-match=uinput`. |
| KVM: pointer doesn't cross with a **touchpad** | Linux capture reads relative devices (mice) via evdev; touchpads aren't supported as the KVM server pointer. Use a mouse. |
| KVM: crossing point feels off (multi-monitor) | Set `[kvm] screen_width/screen_height` to your full desktop size; on Wayland the return position's height isn't restored exactly (X11 is exact). |
| KVM: control won't cross to the Mac | By design, Ubuntu refuses to hand over the keyboard/mouse unless the Mac is *ready to inject* — so a Mac permission problem can never lock you out of Ubuntu. On the Mac run `linkerd check`: the `kvm (daemon)` line must say **ready** (grant Accessibility if it says `NOT READY`). Ubuntu's `linkerd status` `kvm` line stays `server: local` until the Mac is ready. |
| KVM: control released on its own / a "KVM released: …" banner appeared | A safety release fired and the banner names why: the Mac stopped accepting input (Accessibility revoked, the daemon crashed, or the link dropped), the Mac didn't acknowledge the handoff, its heartbeat stopped, or you pressed the panic hotkey. Input is back on Ubuntu immediately — fix the Mac side and cross again. |
| KVM: keyboard/mouse feel stuck after a release | Shouldn't happen — on every release Ubuntu ungrabs the devices (even on crash/shutdown, via an RAII guard) and the Mac releases every held key/modifier. If a modifier seems latched, tap it once. Last resort: the panic hotkey **Ctrl+Alt+Shift+Esc**, or `systemctl --user restart linker`. |
| Clipboard doesn't sync on Wayland | GNOME bridges the Wayland clipboard to Xwayland; Linker uses that. Make sure `DISPLAY` is in `systemctl --user show-environment` (log in to the desktop once). On wlroots/KDE the `wlr-data-control` protocol is used directly. The XDG Desktop Portal clipboard is **not** used (it needs a screen-sharing-style approval each session). |
| `link : Searching` forever | Same network? Not a guest/isolated Wi-Fi ("AP/client isolation" blocks device-to-device traffic — check the router or `ping <mac-ip>`). Firewall: `sudo ufw allow 47800/udp; sudo ufw allow 47801/udp; sudo ufw allow 5353/udp`. If multicast is blocked, set `static_peers = ["<mac-ip>:47800"]` in config.toml on both sides. Logs: `journalctl --user -u linker -f`. |
| `incoming connection rejected … rejected unpaired device` in logs | Another (unpaired) Linker device tried to connect — expected. If it's *your* Mac: you unpaired or reset it; run `linkerd pair` on both. |
| Pairing codes differ | Don't accept. Run `linkerd pair` again on both; ensure no other machine is in pairing mode on the network. |
| No notifications appear | GNOME DND on? (mirrored banners are suppressed while DND is on — see `when_dnd`). Test: `linkerd check`. |
| `linkerd status`: *cannot reach linkerd* | Service not running: `systemctl --user status linker`. |
| Audio: no "Linker Microphone"/"Linker Speaker" in apps | `linkerd status` → `audio` feature line. `pactl info` must report *on PipeWire* (PulseAudio-only systems are not supported: `sudo apt install pipewire-audio` and re-login). Check `systemctl --user status pipewire pipewire-pulse wireplumber`. Linker recreates its devices every 2 s if PipeWire restarts. |
| Audio: build fails (`cmake`, `libspa-sys`, `pipewire-sys`, `clang`) | `sudo apt install cmake libpipewire-0.3-dev libspa-0.2-dev libclang-dev`. |
| Audio: Linker Microphone is silent | Ubuntu `linkerd status`: `audio mic : … peer Unavailable (…)` explains the Mac side (permission denied → see the Mac runbook; `disabled` → `[audio] mic = true` on the Mac). `waiting for audio` → is the Mac awake? The Mac-side `peak -inf dBFS` = the Mac's input is muted. |
| Audio: crackles / dropouts | `linkerd status`: rising `underruns` with large `gap` = Wi-Fi stalls (see the Wi-Fi tips in the macOS runbook; the buffer grows automatically up to `max_buffer_ms`). Rising `PLC` with `loss` > 5 % = bad link: raise `[audio] redundancy = 3`, or `frame_ms = 10` (fewer packets). `cb` ≫ 3 ms = this machine is overloaded. |
| Audio: too much delay | `buf` / `target` show the buffer; it follows the worst packet gap of the last 30 s. On a clean wired/5 GHz link it stays at `min_buffer_ms` (6 ms). Lower `[audio] max_buffer_ms` to cap it (at the cost of dropouts on bad Wi-Fi). |
| Audio: the Mac's mic turns on although no call is running | Something is recording from "Linker Microphone". Level meters count too: pavucontrol's *Input Devices* tab or GNOME Settings → Sound with "Linker Microphone" selected keep it open — close them. `pw-top` shows who is linked. Turn sharing off entirely with `[audio] mic = false` (on either machine). |
| Audio: echo of the remote party | The Mac's speakers are feeding its microphone — use headphones on the Mac, or enable the app's echo cancellation. |
| Audio: test the whole path on one machine | `./runbook/audio-e2e.sh 60` (developer tool: two local daemons, PipeWire on both ends; scenarios `sleepwake`, `restart`, `impair`). |
| Start over | `./runbook/linux/uninstall.sh --purge`, then install and pair again. |
