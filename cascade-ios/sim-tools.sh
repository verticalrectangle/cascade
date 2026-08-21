#!/bin/bash
#
#  sim-tools.sh  —  iOS Simulator automation toolkit for Cascade QA
#
#  Run from the repo root or from ~/Enclave after pulling:
./sim-tools.sh snapshot
#    ./sim-tools.sh click 250 400
#    ./sim-tools.sh type "hello"
#    ./sim-tools.sh paste "link-to-paste"
#    ./sim-tools.sh logs 50
#    ./sim-tools.sh boot
#
set -euo pipefail

# ── config ───────────────────────────────────────────────────────────
DEVICE_UDID="${DEVICE_UDID:-130D2C11-DEC8-4FE0-8272-3275F536CC65}"
BUNDLE_ID="${BUNDLE_ID:-xyz.epsilver.cascade}"
APP_NAME="Cascade"
PROJECT_DIR="${PROJECT_DIR:-$HOME/dev/cascade/cascade-ios}"
CLICK_TOOL="${CLICK_TOOL:-/usr/local/bin/cliclick}"
SCREENSHOT_DIR="${SCREENSHOT_DIR:-/tmp}"
# ─────────────────────────────────────────────────────────────────────

export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"

cd "$PROJECT_DIR" 2>/dev/null || true

usage() {
  cat <<EOF
Usage: $(basename "$0") <command> [args]

Commands:
  boot                          Boot the simulator if it is not already booted.
  snapshot                      Take a screenshot and print the file path.
  click <x> <y> [hold_ms]       Click at absolute screen coordinates.
                                Optional hold_ms performs a long press.
  type <string>                 Type a string into the focused field.
  paste [string]                Copy text (or the Mac clipboard) to the
                                simulator pasteboard.
  logs [--stream] [lines]       Show the last N log lines (default 100).
                                Use --stream to tail the log live.

Environment overrides:
  DEVICE_UDID, BUNDLE_ID, APP_NAME, PROJECT_DIR, CLICK_TOOL, SCREENSHOT_DIR
EOF
}

focus_simulator() {
  osascript -e 'tell application "Simulator" to activate' >/dev/null 2>&1 || true
}

require_cliclick() {
  if ! command -v "$CLICK_TOOL" >/dev/null 2>&1; then
    echo "✗ cliclick not found at $CLICK_TOOL" >&2
    echo "  Install with: brew install cliclick" >&2
    exit 1
  fi
}

sim_boot() {
  if xcrun simctl list devices | grep "$DEVICE_UDID" | grep -q "(Booted)"; then
    echo "Device $DEVICE_UDID is already booted."
    return 0
  fi

  echo "▸ Booting $DEVICE_UDID ..."
  xcrun simctl boot "$DEVICE_UDID"
  open /Applications/Xcode.app/Contents/Developer/Applications/Simulator.app
  xcrun simctl bootstatus "$DEVICE_UDID" -b
  echo "Device $DEVICE_UDID is booted."
}

sim_snapshot() {
  local path="$SCREENSHOT_DIR/enclave_$(date +%Y%m%d_%H%M%S).png"
  echo "▸ Taking screenshot ..."
  xcrun simctl io "$DEVICE_UDID" screenshot "$path"
  echo "$path"
}

sim_click() {
  local x="${1:-}"
  local y="${2:-}"
  local hold_ms="${3:-0}"

  if [[ -z "$x" || -z "$y" ]]; then
    echo "✗ Usage: click <x> <y> [hold_ms]" >&2
    exit 1
  fi

  if ! [[ "$x" =~ ^[0-9]+$ && "$y" =~ ^[0-9]+$ ]]; then
    echo "✗ x and y must be non-negative integers" >&2
    exit 1
  fi

  require_cliclick
  focus_simulator

  if [ "$hold_ms" -gt 0 ]; then
    "$CLICK_TOOL" kd:0 m:"$x,$y" w:"$hold_ms" ku:0
  else
    "$CLICK_TOOL" c:"$x,$y"
  fi
}

sim_type() {
  local text="${1:-}"
  if [ -z "$text" ]; then
    echo "✗ Usage: type <string>" >&2
    exit 1
  fi

  require_cliclick
  focus_simulator
  "$CLICK_TOOL" "t:$text"
}

sim_paste() {
  local text="${1:-}"
  if [ -n "$text" ]; then
    printf '%s' "$text" | xcrun simctl pbcopy "$DEVICE_UDID"
  else
    pbpaste | xcrun simctl pbcopy "$DEVICE_UDID"
  fi
  echo "Pasted into simulator pasteboard."
}

sim_logs() {
  local stream=false
  if [ "${1:-}" = "--stream" ]; then
    stream=true
    shift
  fi
  local lines="${1:-100}"

  if [ "$stream" = true ]; then
    xcrun simctl spawn "$DEVICE_UDID" log stream \
      --predicate 'process == "Enclave"' \
      --info --debug
  else
    xcrun simctl spawn "$DEVICE_UDID" log show \
      --predicate 'process == "Enclave"' \
      --info --debug \
      --last 5m | tail -n "$lines"
  fi
}

# ── dispatch ─────────────────────────────────────────────────────────

case "${1:-}" in
  boot)     sim_boot ;;
  snapshot) sim_snapshot ;;
  click)    shift; sim_click "$@" ;;
  type)     shift; sim_type "$@" ;;
  paste)    shift; sim_paste "$@" ;;
  logs)     shift; sim_logs "$@" ;;
  --help|help|-h) usage ;;
  "")       usage ; exit 1 ;;
  *)        usage ; exit 1 ;;
esac
