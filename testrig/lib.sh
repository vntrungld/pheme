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

# The guest's IPv4 address, from the DHCP lease libvirt handed it. Printed on
# stdout; empty and non-zero if the guest has no lease yet, which is what
# makes it usable as a wait_until condition.
rig_vm_ip() {
    virsh -c qemu:///system domifaddr "$1" 2>/dev/null \
        | awk '/ipv4/ {sub("/.*","",$4); print $4; found=1} END {exit !found}'
}

# True when the guest has a usable graphical session of the kind rig.conf
# names -- not merely an open SSH port. pheme's input capture needs a live
# session, so a VM sitting at a login screen would fail product rows for an
# infrastructure reason. And a VM that silently fell back to X11 after a GDM
# update would run the W rows and report passes for a display server they
# were never testing.
rig_session_ready() {
    _ip=$1; _os=$2
    case "$_os" in
    linux)
        rig_ssh "$_ip" rig "
            d=\$(loginctl show-user rig -p Display --value)
            [ -n \"\$d\" ] || exit 1
            [ \"\$(loginctl show-session \$d -p Type --value)\" = \"$UBUNTU_SESSION\" ]
        "
        ;;
    windows)
        # The marker, plus an interactive session actually present now -- the
        # marker alone survives a reboot into a locked screen.
        rig_ssh "$_ip" rig \
          'if ((Test-Path C:\Users\Public\pheme-rig-ready) -and (Get-Process -Name explorer -ErrorAction SilentlyContinue)) { exit 0 } else { exit 1 }'
        ;;
    *) return 1 ;;
    esac
}

# Start a domain and wait until it is genuinely usable. Prints the IP.
rig_boot() {
    _dom=$1; _os=$2
    virsh -c qemu:///system start "$_dom" >/dev/null 2>&1 || true
    wait_until 180 "$_dom to get an address" rig_vm_ip "$_dom" \
        || { log "rig error: $_dom never got an address"; return 1; }
    _ip=$(rig_vm_ip "$_dom")
    wait_until 300 "$_dom to reach a usable session" rig_session_ready "$_ip" "$_os" \
        || { log "rig error: $_dom has no graphical session"; return 1; }
    printf '%s\n' "$_ip"
}

# The canary proves a revert actually happened. The runner drops it at the end
# of every run; after the next revert it must be gone. A snapshot-revert that
# silently did nothing would otherwise leave the rig running rows against the
# previous run's leftovers and reporting the result as fact.
#
# It lives in the home directory, not /tmp: Ubuntu empties /tmp at every boot,
# so a canary there would vanish whether or not the revert happened, and the
# check would pass on exactly the failure it exists to catch. And "absent"
# must be the guest answering so -- an ssh that failed is not evidence.
#
# Usage: rig_canary_write <ip> <user> <linux|windows>. One command per OS: the
# Windows side runs PowerShell 5.1, which has no `||`.
rig_canary_write() {
    case "$3" in
    linux) rig_ssh "$1" "$2" 'touch "$HOME/.pheme-rig-canary"' ;;
    windows) rig_ssh "$1" "$2" 'New-Item -ItemType File -Force -Path $env:USERPROFILE\.pheme-rig-canary | Out-Null' ;;
    *) return 1 ;;
    esac
}
rig_canary_absent() {
    case "$3" in
    linux) rig_ssh "$1" "$2" 'test ! -e "$HOME/.pheme-rig-canary"' ;;
    windows) rig_ssh "$1" "$2" 'if (Test-Path $env:USERPROFILE\.pheme-rig-canary) { exit 1 } else { exit 0 }' ;;
    *) return 1 ;;
    esac
}
