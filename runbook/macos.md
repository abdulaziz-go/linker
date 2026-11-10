# Linker on macOS — Runbook

The Mac is the **KVM client** (it's controlled from Ubuntu's keyboard/mouse),
the **camera source** and the **audio source**: its microphone becomes
"Linker Microphone" on Ubuntu, and whatever Ubuntu apps play to "Linker
Speaker" is played on the Mac's current output (speakers / headphones /
AirPods). Everything runs in userspace — no kernel or
system extensions, no sudo. Tested target: macOS 13–15, Apple silicon or Intel.

> Do the Ubuntu side too ([ubuntu.md](ubuntu.md)). Pairing needs both machines.

---

## 1. Prerequisites

```bash
xcode-select --install                       # Command Line Tools (compiler, codesign)
brew install cmake                           # builds the bundled Opus audio codec (install.sh does this if Homebrew is present)
curl https://sh.rustup.rs -sSf | sh -s -- -y # Rust
source "$HOME/.cargo/env"
```

Same Wi-Fi/LAN as the Ubuntu machine (not a guest network).

## 2. Build, install, start at login (one command)

```bash
./runbook/macos/install.sh
```

What it does (idempotent — rerun after `git pull` to upgrade):

1. `cargo build --release -p linker-daemon`
2. Installs `~/.local/bin/linkerd` and ad-hoc signs it with identifier
   `io.linker.daemon`.
3. Writes a commented config: `~/Library/Application Support/linker/config.toml`.
4. Installs the **LaunchAgent** `~/Library/LaunchAgents/io.linker.daemon.plist`
   (`RunAtLoad` + `KeepAlive`, GUI session only) and loads it with
   `launchctl bootstrap gui/$(id -u)`.

Service control:

```bash
launchctl print gui/$(id -u)/io.linker.daemon | head -20   # state
tail -f ~/Library/Logs/linker.log                            # logs
launchctl kickstart -k gui/$(id -u)/io.linker.daemon         # restart (after permission/config changes)
```

## 3. Create the two DND Shortcuts

macOS has no API to switch Focus, so Linker runs two Shortcuts you create once:

1. Open **Shortcuts** → **+** (new shortcut). Name it exactly **`Linker DND On`**.
2. Add action **Set Focus** → choose **Do Not Disturb** → **Turn On** → **Until Turned Off**.
3. New shortcut **`Linker DND Off`** with **Set Focus** → **Do Not Disturb** → **Turn Off**.
4. Test in Terminal:
   ```bash
   shortcuts run "Linker DND On";  shortcuts run "Linker DND Off"
   ```

## 4. Grant permissions

Open **System Settings → Privacy & Security** and add
`~/.local/bin/linkerd` where a **+** button exists (in the file picker press
**⌘⇧G** and paste `~/.local/bin/linkerd`; **⌘⇧.** shows hidden folders):

| Setting | Needed for | How |
|---|---|---|
| **Accessibility** | KVM: injecting keyboard/mouse (and the event tap) | + → linkerd, switch on |
| **Input Monitoring** | KVM capture (only if you make the Mac the KVM *server*) | + → linkerd, switch on |
| **Full Disk Access** | reading the Notification Center database (mirroring) and Focus state (DND sync Mac→Ubuntu) | + → linkerd, switch on |
| **Camera** | camera sharing | No + button: macOS **asks** the first time an Ubuntu app opens "Linker Camera" — click **Allow**. |
| **Microphone** | microphone sharing ("Linker Microphone" on Ubuntu) | No + button: macOS **asks** the first time an Ubuntu app records from "Linker Microphone" (or on `linkerd check`) — click **Allow**. The binary carries a usage description ("Linker shares this Mac's microphone…") so the prompt names the reason. |
| **Local Network** (macOS 15+) | talking to Ubuntu on the LAN | macOS **asks** on first connection — click **Allow**. |
| **Notifications → Script Editor** | Linker's own banners (e.g. "peer not found") | make sure *Allow notifications* is on |
| Firewall prompt "accept incoming connections?" | inbound link | **Allow** |

Then restart the agent so it picks everything up:

```bash
launchctl kickstart -k gui/$(id -u)/io.linker.daemon
linkerd check
```

`linkerd check` should show `[ OK ]` for each line. On first run it also
briefly opens the camera and the microphone to trigger the Camera/Microphone
prompts *for Terminal*; the daemon itself gets its own prompts the first time
it streams. Audio lines look like:

```
  [ OK ] audio mic capture      "MacBook Pro Microphone" 48000 Hz 1 ch, period default, max callback gap 10.9 ms, level -38.2 dBFS (2.0s captured)
  [ OK ] audio output           "MacBook Pro Speakers" 48000 Hz 2 ch, period 128 frames (2.67 ms), device latency 9.8 ms, max callback gap 3.1 ms
  [ OK ] audio engine           Opus LOWDELAY 5 ms frames ×2 redundancy, buffer 6–200 ms: engine latency 9.5 ms (excl. devices + network), gaps 0
```

The output test plays **silence** (nothing is heard). `level -inf dBFS` on the
mic line (`[WARN] … digital silence`) almost always means the Microphone
permission is missing or the input is muted.

## 5. Pair with Ubuntu (first time only)

On **both** machines, within 2 minutes:

```bash
linkerd pair
```

Compare the 6-digit codes on the two screens. Same → `y` on both. Different →
`n` and retry. After `✔ Paired`, the link connects automatically from now on —
after reboots, lid close/open and Wi-Fi changes. Only the paired Ubuntu
machine can connect. `linkerd unpair` forgets it.

## 6. Using each feature

| Feature | How to use |
|---|---|
| **Clipboard** | ⌘C on the Mac → Ctrl+V on Ubuntu, and back. Text, rich text, images, files. Copies from your iPhone reach the Mac via Apple's Universal Clipboard and continue to Ubuntu. Password-manager copies (marked *concealed*) are **not** synced (`[clipboard] skip_concealed`). Received files are saved in `~/Downloads/Linker/`. |
| **Notifications** | Mac notifications are mirrored to Ubuntu automatically. Exclude apps with `[notifications] ignore_apps = ["Mail"]`. |
| **Do Not Disturb** | Toggle Do Not Disturb in Control Center → Ubuntu follows; Ubuntu's DND switches the Mac via the Shortcuts. |
| **KVM** | Move Ubuntu's mouse past its right edge → the pointer appears at the Mac's left edge. Type away. Move back out the Mac's left edge to return. Ctrl from the Linux keyboard acts as ⌘ (`[kvm] swap_ctrl_cmd`). |
| **Camera** | Nothing to do on the Mac: when an Ubuntu app selects "Linker Camera", the camera turns on (green light) and turns off ~5 s after the app stops. |
| **Microphone** | Nothing to do on the Mac: when an Ubuntu app (Zoom, a browser…) records from "Linker Microphone", the Mac's **default input** turns on (orange dot in the menu bar) and turns off ~5 s after the app stops. Pick another mic with `[audio] input_device = "…"` (name as in System Settings → Sound → Input); otherwise Linker follows the default input, also when you change it. |
| **Speaker** | Ubuntu apps whose output is "Linker Speaker" play on the Mac's **default output** with ~2.7 ms callbacks (low latency). It follows output changes (headphones, AirPods) automatically and releases the device 10 s after the audio stops (`[audio] idle_close_secs`). Codec/buffer settings of this stream are in **this** Mac's `[audio]` section (`bitrate_kbps`, `frame_ms`, `redundancy`, `min/max_buffer_ms`, `output_device`). Use headphones during calls to avoid echo. |

## 7. Verify it works

```bash
linkerd status
```

- `link : Connected`, peer = your Ubuntu machine.
- **Clipboard**: copy text here, paste on Ubuntu; copy a file in Finder, paste
  in Ubuntu's Files.
- **Notifications**: `osascript -e 'display notification "hello from the Mac" with title "Test"'`
  → a banner "ScriptEditor2 — Test" appears on Ubuntu within ~1 s (banners
  whose title starts with "Linker" are Linker's own and are not mirrored).
  A Message or Mail notification works the same way.
- **DND**: turn on Do Not Disturb → Ubuntu's quick settings show DND within ~1 s.
- **Camera**: on Ubuntu open "Linker Camera" in a browser; the green light
  turns on here; `linkerd status` on Ubuntu shows fps/latency.
- **Microphone**: on Ubuntu `pw-record --target linker_microphone /tmp/m.wav`
  (or Zoom → *Test Mic* with "Linker Microphone"): the orange dot appears
  here; `linkerd status` here shows `audio mic : send streaming … peak …`.
- **Speaker**: on Ubuntu `pw-play --target linker_speaker some.wav` (or Zoom →
  *Test Speaker* with "Linker Speaker"): it plays on this Mac; `linkerd status`
  here shows `audio speaker : recv playing | buf …`.
- **Pairing/security**: `linkerd status` shows `paired with` = Ubuntu's id.

## 8. Troubleshooting

| Symptom | Fix |
|---|---|
| `kvm … not trusted for Accessibility` / `cannot create event tap` | Add `~/.local/bin/linkerd` to Accessibility (and Input Monitoring), then `launchctl kickstart -k gui/$(id -u)/io.linker.daemon`. |
| **Permissions reset after updating** | Rebuilding changes the binary's ad-hoc signature, and macOS treats it as a new app. Remove linkerd from the lists (–) and add it again, then restart the agent. |
| Camera: `permission denied` | System Settings → Privacy & Security → Camera → enable linkerd. If it's not listed / you clicked Don't Allow: `tccutil reset Camera`, restart the agent, open the camera from Ubuntu again and click Allow. |
| Camera: Ubuntu shows the placeholder (gray card) | Mac asleep/lid closed, or `Busy` in Ubuntu's `linkerd status` (another app holding the camera exclusively). It resumes automatically. |
| Microphone: Ubuntu shows `peer Unavailable (microphone permission denied …)` | System Settings → Privacy & Security → **Microphone** → enable linkerd (or Terminal when running by hand). Not listed / clicked Don't Allow: `tccutil reset Microphone`, `launchctl kickstart -k gui/$(id -u)/io.linker.daemon`, record from "Linker Microphone" on Ubuntu again and click Allow. After a rebuild, permissions may need re-granting (see *Permissions reset after updating*). |
| Microphone: Ubuntu receives silence | Mac-side `linkerd status` → `audio mic … peak -inf dBFS`: input muted / wrong device. Check System Settings → Sound → Input (level meter moves?), or set `[audio] input_device`. |
| Speaker: nothing plays on the Mac | Mac `linkerd status`: no `audio speaker` line = no audio is arriving (is the Ubuntu app's output "Linker Speaker"? the app must be playing). `recv playing` but silent: check the Mac's output volume / selected output. `audio speaker … unavailable` in the log: set `[audio] output_device` or check System Settings → Sound → Output. |
| Speaker: crackles | `linkerd status` here: `cb` ≫ 3 ms = the Mac is busy; rising `underruns` with large `gap` = Wi-Fi stalls (the buffer adapts up to `max_buffer_ms`). Wi-Fi tips: use 5 GHz (Option-click the Wi-Fi icon → *Channel*), wired Ubuntu, and AirDrop/AWDL causes periodic stalls: `sudo ifconfig awdl0 down` (until reboot). |
| Build fails: `cmake` not found (opusic-sys) | `brew install cmake`, rerun `./runbook/macos/install.sh`. |
| `shortcut "Linker DND On" failed` | Create the Shortcuts exactly as in §3 (names are case-sensitive). Test with `shortcuts run "Linker DND On"`. |
| DND changes on the Mac don't reach Ubuntu | Needs Full Disk Access (reads `~/Library/DoNotDisturb/DB/Assertions.json`, macOS 12+). Scheduled/automatic Focus modes aren't detected — only manual toggles. Without it, DND still works Ubuntu → Mac ("set-only"). |
| Notifications not mirrored | Needs Full Disk Access. The database location changed in macOS 15 (`~/Library/Group Containers/group.com.apple.usernoted/db2/db`, older: `$(getconf DARWIN_USER_DIR)/com.apple.notificationcenter/db2/db`). This relies on a private format and may need an update after a major macOS release — check `linkerd status` → `notifications`. |
| `link : Searching` forever | Same network? Not a guest Wi-Fi with client isolation. macOS 15: System Settings → Privacy & Security → **Local Network** → enable linkerd. Firewall: allow linkerd. If multicast is blocked, set `static_peers = ["<ubuntu-ip>:47800"]` in config.toml on both. Logs: `tail -f ~/Library/Logs/linker.log`. |
| `No route to host` in logs (macOS 15) | Local Network permission denied — enable it (above), restart the agent. |
| Keychain prompt "linkerd wants to use your confidential information" | Click **Always Allow** (the identity key lives in your login Keychain). After a rebuild it may ask once more. |
| Start over | `./runbook/macos/uninstall.sh --purge`, then install and pair again. |
