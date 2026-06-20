#!/usr/bin/env bash
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
LOGDIR="$DIR/logs"
mkdir -p "$LOGDIR"
LOG_OUT="$LOGDIR/server-stdout.log"
LOG_ERR="$LOGDIR/server-stderr.log"

# Rotate old logs (keep one previous copy)
for f in "$LOG_OUT" "$LOG_ERR"; do
    if [[ -f "$f" ]]; then
        mv "$f" "$f.old"
    fi
done

echo "▶ Starting goop server (features: cuda)…"
echo "   stdout → $LOG_OUT"
echo "   stderr → $LOG_ERR"

# Launch in background with nohup so SIGHUP is ignored.
# cargo run rebuilds if needed, then execs the binary —
# we want the eventual server to survive hangup.
nohup nix develop --command cargo run --features cuda -- serve \
    >"$LOG_OUT" \
    2>"$LOG_ERR" &
PID=$!

echo "   pid    → $PID"
echo "   wait   → wait $PID"
echo "   tail   → tail -f $LOG_OUT"

# Write PID file so the caller can kill later
echo "$PID" > "$LOGDIR/server.pid"
