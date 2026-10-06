# Shared helpers. Sourced, never executed.
# shellcheck shell=sh

log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die() { printf 'rig error: %s\n' "$*" >&2; exit 1; }

# wait_until <deadline_seconds> <description> <command...>
#
# Polls the command until it succeeds or the deadline passes. Returns 0 on
# success, 1 on timeout.
#
# This exists because there is no `sleep` in this rig. A fixed wait either
# flakes under load or -- worse -- passes on an idle machine while proving
# nothing, which is a defect this project has already shipped once in a
# sub-project 7 test. Every wait in the rig goes through here so that the
# deadline is always explicit and always reported.
wait_until() {
    _deadline=$1; _what=$2; shift 2
    _end=$(( $(date +%s) + _deadline ))
    while :; do
        if "$@" >/dev/null 2>&1; then return 0; fi
        if [ "$(date +%s)" -ge "$_end" ]; then
            log "timed out after ${_deadline}s waiting for: $_what"
            return 1
        fi
        sleep 1   # the poll interval, not a guess at how long the work takes
    done
}

# Only one run at a time. Two overlapping runs would fight over the same
# domains and the same network and produce a report mixing both, which is
# worse than refusing.
rig_lock() {
    mkdir -p "$RIG_STATE"
    if ! mkdir "$RIG_STATE/lock" 2>/dev/null; then
        die "another rig run holds $RIG_STATE/lock; remove it if no run is active"
    fi
    echo $$ > "$RIG_STATE/lock/pid"
}
rig_unlock() { rm -rf "$RIG_STATE/lock"; }

# ssh into a rig VM by IP, with the rig's own key and no host-key prompt:
# these are throwaway machines rebuilt from snapshots, so a changing host key
# is expected rather than suspicious.
rig_ssh() {
    _ip=$1; _user=$2; shift 2
    ssh -i "$RIG_SSH_KEY" \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=5 \
        "$_user@$_ip" "$@"
}
