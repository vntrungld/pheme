#!/bin/sh
# Tests for testrig/lib.sh. Run: sh testrig/lib_test.sh
set -eu
cd "$(dirname "$0")"
. ./lib.sh

fail=0
check() {
    if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want $3, got $2"; fail=1; fi
}

# A condition that is already true returns at once.
wait_until 5 "true immediately" true
check "already-true returns 0" "$?" "0"

# A condition that never becomes true returns non-zero, and does so *before*
# twice the deadline -- a wait_until that ignored its deadline would hang the
# whole rig, which is the failure this test exists for.
start=$(date +%s)
if wait_until 2 "never true" false; then rc=0; else rc=1; fi
elapsed=$(( $(date +%s) - start ))
check "never-true returns 1" "$rc" "1"
if [ "$elapsed" -le 4 ]; then echo "ok   respects the deadline ($elapsed s)"; else
    echo "FAIL deadline ignored: $elapsed s for a 2 s deadline"; fail=1; fi

# A condition that becomes true partway through is noticed.
marker=$(mktemp)
rm -f "$marker"
( sleep 1; touch "$marker" ) &
wait_until 5 "marker appears" test -f "$marker"
check "eventually-true returns 0" "$?" "0"
rm -f "$marker"

exit "$fail"
