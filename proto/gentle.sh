#!/usr/bin/env bash
ulimit -v "${GENTLE_VMEM_KB:-3145728}"
exec systemd-run --user --scope -q -p CPUQuota=100% -- nice -n 19 ionice -c3 "$@"
