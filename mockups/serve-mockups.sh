#!/usr/bin/env bash
# Serves the ./mockups folder on a port (default 8090), bound to 0.0.0.0
# so any LAN device can reach it. Falls back through 8090..8099.
# Prints the chosen LAN URL.

set -euo pipefail

cd "$(dirname "$0")"

PORT=8090
while [ $PORT -lt 8100 ]; do
  if ! ss -tln 2>/dev/null | grep -q ":$PORT "; then
    break
  fi
  PORT=$((PORT + 1))
done

if [ $PORT -eq 8100 ]; then
  echo "ERROR: no free port in 8090..8099" >&2
  exit 1
fi

# Best-effort LAN IP discovery.
LAN_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
if [ -z "$LAN_IP" ]; then
  LAN_IP="127.0.0.1"
fi

echo "mockups dir : $(pwd)"
echo "LAN URL     : http://${LAN_IP}:${PORT}/"
echo "Local URL   : http://localhost:${PORT}/"
echo
echo "Serving... press Ctrl-C to stop."

exec python3 -m http.server "$PORT" --bind 0.0.0.0
