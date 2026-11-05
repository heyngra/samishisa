#!/usr/bin/env bash
PG=$1; PAUSED=0
while kill -0 -- "-$PG" 2>/dev/null; do
  avail=$(awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo)
  if [ "$PAUSED" = 0 ] && [ "$avail" -lt 700 ]; then kill -STOP -- "-$PG"; PAUSED=1; echo "$(date +%T) paused build (MemAvailable ${avail} MB)"; fi
  if [ "$PAUSED" = 1 ] && [ "$avail" -gt 1200 ]; then kill -CONT -- "-$PG"; PAUSED=0; echo "$(date +%T) resumed build (MemAvailable ${avail} MB)"; fi
  sleep 2
done
