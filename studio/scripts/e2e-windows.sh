#!/usr/bin/env bash
# GPU end-to-end checks for jai.exe on Windows, driven from WSL.
#
#   studio/scripts/e2e-windows.sh [--home <windows path>] [--skip-download] [--generations N]
#
# Uses the real data folder (%LOCALAPPDATA%\JAI) unless --home is given. It installs what is
# missing (hard-linking LM Studio copies), starts its own `jai serve`, and checks the HTTP rules,
# image generation, chat with an image (an engine swap), rendering a .jai, a short encode, and that
# no engine outlives jai.exe. Prints PASS or FAIL per check; exits non-zero on any failure.
#
# Nothing else may run a JAI server on that data folder (JAI Studio, its tray or another
# `jai serve`): the checks would test that server instead. The script stops only the `jai serve`
# it started, by its pid.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
tools=$(cd "$here/../.." && pwd)
exe_lin="$tools/dist/studio/jai.exe"
home_win=""
skip_download=0
generations=2
while [ $# -gt 0 ]; do
  case "$1" in
    --home) home_win="$2"; shift 2 ;;
    --skip-download) skip_download=1; shift ;;
    --generations) generations="$2"; shift 2 ;;
    *) echo "unknown option $1"; exit 2 ;;
  esac
done
[ -f "$exe_lin" ] || { echo "missing $exe_lin: run studio/scripts/build-windows.sh"; exit 1; }
exe=$(wslpath -w "$exe_lin")
out_lin=$(mktemp -d "$(wslpath "$(cmd.exe /c 'echo %TEMP%' 2>/dev/null | tr -d '\r')")/jai-e2e.XXXX")
out=$(wslpath -w "$out_lin")

pass=0; fail=0
check() { if [ "$1" = 0 ]; then echo "PASS  $2"; pass=$((pass+1)); else echo "FAIL  $2"; fail=$((fail+1)); fi; }
# Run a Windows command line from a .cmd file, so quoting reaches Windows intact.
win() {
  { printf '@echo off\r\n'; [ -n "$home_win" ] && printf 'set JAI_HOME=%s\r\n' "$home_win"; printf '%s\r\n' "$1"; } > "$out_lin/step.cmd"
  cmd.exe /c "$out\\step.cmd" 2>&1 | tr -d '\r'
}
jai() { win "\"$exe\" $*"; }
http() { win "curl.exe -s -o NUL -w %%{http_code} $*"; }

echo "jai.exe: $exe"; echo "output:  $out"
v=$(jai --version); echo "$v"; [[ "$v" == jai* ]]; check $? "--version"

if [ $skip_download = 0 ]; then
  t0=$SECONDS; jai models download --yes > "$out_lin/download.txt"; check $? "models download ($((SECONDS-t0)) s)"
fi
jai models status > "$out_lin/models-status.txt"
# Every state other than installed ("missing", "partial", "on this PC") leaves this line out.
grep -q "^Everything is installed" "$out_lin/models-status.txt"; check $? "every component installed"

if jai status | grep -q "^Server: *running"; then
  echo "FAIL  a JAI server already runs on this data folder: quit JAI Studio (and its tray) or stop"
  echo "      that \`jai serve\`, or pass --home <folder> to use another data folder"
  exit 1
fi
serve_pid=""
# Stops the server this script started, however the script ends; nothing else.
cleanup() { [ -n "$serve_pid" ] && taskkill.exe /PID "$serve_pid" /F > /dev/null 2>&1; }
trap cleanup EXIT
# Start-Process, not cmd's start: a process started from cmd.exe inherits the pipe WSL reads
# its output from, and WSL then waits for jai serve to exit.
ps_start="Start-Process -WindowStyle Minimized -FilePath '$exe' -ArgumentList 'serve'"
[ -n "$home_win" ] && ps_start="\$env:JAI_HOME='$home_win'; $ps_start"
powershell.exe -NoProfile -Command "$ps_start" > /dev/null 2>&1
for _ in $(seq 1 30); do
  serve_pid=$(jai status | sed -n 's/^Server: *running at .* (pid \([0-9]*\),.*/\1/p')
  [ -n "$serve_pid" ] && break
  sleep 1
done
# No server ran before, so the one that answers is ours; it must be a jai.exe.
[ -n "$serve_pid" ] && tasklist.exe /FI "PID eq $serve_pid" /NH 2>/dev/null | tr -d '\r' | grep -qi "^jai\.exe"
check $? "jai serve started (pid ${serve_pid:-none})"
url=$(jai token | sed -n 's/^URL: *//p'); token=$(jai token | sed -n 's/^Token: *//p')
[ -n "$url" ] && [ -n "$token" ]; check $? "server up at $url"

[ "$(http "$url/v1/models")" = 401 ]; check $? "401 without a token"
[ "$(http -H "\"Authorization: Bearer $token\"" "$url/v1/models")" = 200 ]; check $? "200 with the token"
[ "$(http -H "\"Host: evil.example\"" "$url/health")" = 403 ]; check $? "403 for a foreign Host header"
[ "$(http -H "\"Origin: https://evil.example\"" -H "\"Authorization: Bearer $token\"" "$url/v1/models")" = 403 ]; check $? "403 for a web Origin"

t0=$SECONDS; jai generate "\"Photograph of a red bicycle leaning on a white wall, afternoon sun\"" --seed 7 --out "\"$out\\bicycle.png\"" > "$out_lin/generate.txt"
[ -s "$out_lin/bicycle.png" ]; check $? "generate an image ($((SECONDS-t0)) s, includes starting the image engine)"

t0=$SECONDS; jai chat "\"In one word: what colour is the bicycle?\"" --image "\"$out\\bicycle.png\"" > "$out_lin/chat.txt"
grep -qi "red" "$out_lin/chat.txt"; check $? "chat about the image ($((SECONDS-t0)) s, includes a swap to the LLM)"

doc=$(wslpath -w "$tools/../jai/examples/encoding/earthrise.jai")
t0=$SECONDS; jai render "\"$doc\"" --seed 1 --out "\"$out\\earthrise.png\"" > "$out_lin/render.txt"
[ -s "$out_lin/earthrise.png" ]; check $? "render earthrise.jai ($((SECONDS-t0)) s, includes a swap back)"

target=$(wslpath -w "$tools/../jai/docs/assets/images/encoding/earthrise-target.webp")
t0=$SECONDS; jai encode "\"$target\"" --generations "$generations" --out "\"$out\\earthrise.jai\"" --force > "$out_lin/encode.txt"
[ -s "$out_lin/earthrise.jai" ]; check $? "encode Earthrise, $generations generations ($((SECONDS-t0)) s)"
grep -E "^ +black-forest-labs|^Stopped" "$out_lin/encode.txt"

jai status > "$out_lin/status.txt"; sed -n 2p "$out_lin/status.txt"
grep -qE "[1-9][0-9]* swaps?" "$out_lin/status.txt"; check $? "the scheduler swapped engines"

# The engines our server runs now (the encode leaves one loaded), then a hard kill of that
# server alone: its Job object must take the engines with it.
engines=$(win "powershell.exe -NoProfile -Command \"Get-CimInstance Win32_Process -Filter 'ParentProcessId=$serve_pid' | Where-Object { \$_.Name -match 'llama-server|sd-server' } | ForEach-Object { \$_.ProcessId }\"" | tr -s ' \n' ' ')
[ -n "${engines// /}" ]; check $? "an engine runs under jai serve (pid ${engines:-none})"
taskkill.exe /PID "$serve_pid" /F > /dev/null 2>&1; sleep 2
left=0
for p in $engines; do
  tasklist.exe /FI "PID eq $p" /NH 2>/dev/null | tr -d '\r' | grep -q " $p " && left=$((left+1))
done
[ "$left" = 0 ]; check $? "no engine outlives jai.exe ($left left)"
serve_pid=""

echo; echo "$pass passed, $fail failed. Output in $out"
[ $fail = 0 ]
