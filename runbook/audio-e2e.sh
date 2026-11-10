#!/usr/bin/env bash
# Linker — audio end-to-end run on ONE Ubuntu machine (developer tool).
#
# Starts two real linkerd processes talking QUIC over localhost:
#   U = Ubuntu role (virtual "Linker Microphone" + "Linker Speaker")
#   M = stands in for the Mac: its "speakers" are a PipeWire null sink and its
#       "microphone" is that sink's monitor — so whatever U sends to M's
#       speakers comes straight back as M's microphone.
# Then a Zoom-like test app plays a tone into "Linker Speaker" and records
# "Linker Microphone": the sound crosses BOTH flows (B then A) over the real
# encrypted link, through real PipeWire on both ends.
#
#   ./runbook/audio-e2e.sh [seconds] [scenario]
#   scenario: steady (default) | sleepwake | restart | impair | churn
#   (churn: another sink appears/disappears + a short sound every 20 s —
#    PipeWire graph reconfigurations during the call)
#
# Everything lives in a temp dir; your real config, keys and running service
# are not touched. Clipboard, notifications, DND, KVM and camera are disabled
# in both test daemons.
set -euo pipefail
node_id() { pw-cli ls Node | awk -v n="\"$1\"" '/^[[:space:]]*id [0-9]+,/{gsub(",","",$2); id=$2} $0 ~ "node.name = " n {print id}'; }
SECS="${1:-60}"
SCENARIO="${2:-steady}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${LINKERD:-$REPO/target/release/linkerd}"
WORK="$(mktemp -d /tmp/linker-audio-e2e.XXXXXX)"
U="$WORK/u"; M="$WORK/m"; mkdir -p "$U" "$M"
PU=48900; PM=48910

common='mdns = false
broadcast = false
keystore = "file"
not_found_grace_secs = 3600
[clipboard]
enabled = false
[notifications]
enabled = false
[dnd]
enabled = false
[kvm]
enabled = false
[camera]
enabled = false'

cat >"$U/config.toml" <<EOF
device_name = "e2e-ubuntu"
port = $PU
discovery_port = $((PU+1))
static_peers = ["127.0.0.1:$PM"]
$common
[audio]
role = "sink"
# test-only device names: never collide with an installed linkerd's
# "Linker Microphone"/"Linker Speaker" (which may be linked to your real Mac)
mic_node = "linker_e2e_microphone"
speaker_node = "linker_e2e_speaker"
mic_name = "Linker E2E Microphone"
speaker_name = "Linker E2E Speaker"
EOF
cat >"$M/config.toml" <<EOF
device_name = "e2e-mac"
port = $PM
discovery_port = $((PM+1))
static_peers = ["127.0.0.1:$PU"]
$common
[audio]
role = "source"
input_device = "linker_e2e_mac_out.monitor"
output_device = "linker_e2e_mac_out"
EOF

cleanup() {
  set +e
  [[ -n "${CHURN:-}" ]] && kill "$CHURN" 2>/dev/null || true
  id=$(node_id linker_e2e_churn 2>/dev/null | head -1); [[ -n "$id" ]] && pw-cli destroy "$id" >/dev/null 2>&1 || true
  [[ -n "${PID_U:-}" ]] && kill "$PID_U" 2>/dev/null || true
  if [[ -f "$WORK/m.pid" ]]; then PID_M=$(cat "$WORK/m.pid"); fi
  if [[ -n "${PID_M:-}" ]]; then kill -CONT "$PID_M" 2>/dev/null; kill "$PID_M" 2>/dev/null; fi
  [[ -n "${SINK_ID:-}" ]] && pw-cli destroy "$SINK_ID" >/dev/null 2>&1
  for n in linker_e2e_mac_out linker_e2e_churn; do
    for id in $(node_id "$n" 2>/dev/null); do pw-cli destroy "$id" >/dev/null 2>&1; done
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT

echo "==> null sink standing in for the Mac's speakers"
pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=linker_e2e_mac_out node.description="E2E Mac speakers" media.class=Audio/Sink object.linger=true audio.position=[FL FR] audio.rate=48000 }' >/dev/null
sleep 0.5
SINK_ID=$(node_id linker_e2e_mac_out | head -1)
[[ -n "$SINK_ID" ]] || { echo "!! could not create the null sink"; exit 1; }
echo "    node id $SINK_ID"

echo "==> identities + pairing (pre-pinned, no interactive pairing)"
for d in "$U" "$M"; do
  "$BIN" --config-dir "$d" run >"$d/first.log" 2>&1 & p=$!; sleep 1.5; kill $p; wait $p 2>/dev/null || true
done
id_of() { awk -F'"' '/device_id/{print $2}' "$1/identity.toml"; }
IDU=$(id_of "$U"); IDM=$(id_of "$M")
printf '[peer]\ndevice_id = "%s"\ndevice_name = "e2e-mac"\ndevice_type = "laptop"\npaired_at_ms = 0\n' "$IDM" >"$U/paired.toml"
printf '[peer]\ndevice_id = "%s"\ndevice_name = "e2e-ubuntu"\ndevice_type = "desktop"\npaired_at_ms = 0\n' "$IDU" >"$M/paired.toml"

if [[ "$SCENARIO" == impair ]]; then
  export LINKER_AUDIO_IMPAIR="${LINKER_AUDIO_IMPAIR:-loss=5,reorder=10,jitter=5,stall_every=1500,stall_ms=80,seed=3}"
  echo "==> impairment on both send paths: $LINKER_AUDIO_IMPAIR"
fi
echo "==> starting daemons (logs in $WORK)"
export RUST_LOG="${RUST_LOG:-info}"
"$BIN" --config-dir "$U" run >"$U/linkerd.log" 2>&1 & PID_U=$!
"$BIN" --config-dir "$M" run >"$M/linkerd.log" 2>&1 & PID_M=$!
for _ in $(seq 1 40); do
  if "$BIN" --config-dir "$U" status 2>/dev/null | grep -q "link        : Connected"; then break; fi
  sleep 0.5
done
"$BIN" --config-dir "$U" status | sed -n '1,8p'

if command -v pactl >/dev/null; then
  echo "==> what PulseAudio apps (Zoom, Chrome, Firefox) see:"
  pactl list short sources | grep -i linker_e2e || true
  pactl list short sinks | grep -i linker_e2e || true
fi

case "$SCENARIO" in
  sleepwake) ( sleep $((SECS/3)); echo "==> [t+$((SECS/3))s] M sleeps (SIGSTOP 20 s)"; kill -STOP "$PID_M"; sleep 20; echo "==> M wakes (SIGCONT)"; kill -CONT "$PID_M" ) & ;;
  restart)   ( sleep $((SECS/3)); echo "==> [t+$((SECS/3))s] M crashes and restarts"; kill -9 "$PID_M"; sleep 3
               "$BIN" --config-dir "$M" run >>"$M/linkerd.log" 2>&1 & echo $! >"$WORK/m.pid" ) & ;;
  churn)     ( while sleep 20; do
                 pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=linker_e2e_churn media.class=Audio/Sink object.linger=true }' >/dev/null
                 sleep 1; timeout 2 pw-cat --playback --target linker_e2e_churn /usr/share/sounds/alsa/Front_Center.wav >/dev/null 2>&1 || true
                 id=$(node_id linker_e2e_churn | head -1); [[ -n "$id" ]] && pw-cli destroy "$id" >/dev/null 2>&1
                 echo "==> graph churn"
               done ) & CHURN=$! ;;
esac

echo "==> Zoom-like app: tone → Linker Speaker … Linker Microphone → recorder ($SECS s)"
( cd "$REPO" && LINKER_E2E=1 LINKER_E2E_SECS="$SECS" LINKER_AUDIO_IMPAIR= \
    LINKER_E2E_SPEAKER=linker_e2e_speaker LINKER_E2E_MIC=linker_e2e_microphone \
    cargo test -q --release -p linker-linux --test e2e_daemons -- --ignored --nocapture 2>&1 \
    | grep -vE '^(running|test |$)|test result' ) || true

[[ -f "$WORK/m.pid" ]] && PID_M=$(cat "$WORK/m.pid")
echo "==> linkerd status (Ubuntu side)"; "$BIN" --config-dir "$U" status | grep -E "^audio|link|features" -A0 || true
echo "==> linkerd status (Mac side)";    "$BIN" --config-dir "$M" status | grep -E "^audio|link" || true
echo "==> logs: $WORK/{u,m}/linkerd.log"
