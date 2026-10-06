#!/bin/sh
# Create or revert each VM's clean snapshot.
# Usage: snapshot.sh create <domain> | revert <domain>
#
# The revert is what makes a second run mean anything. Rows install and remove
# packages, write trusted.toml during pairing and config.toml on first start.
# Without reverting, a later run can pass on the previous run's leftovers
# rather than on the code.
set -eu
cd "$(dirname "$0")"
. ./rig.conf
. ./lib.sh

VIRSH="virsh -c qemu:///system"
SNAP=clean
DOM="${2:?usage: snapshot.sh create|revert <domain>}"

case "${1:-}" in
create)
    $VIRSH destroy "$DOM" 2>/dev/null || true
    wait_until 60 "$DOM to stop" sh -c "$VIRSH domstate $DOM | grep -q 'shut off'" \
        || die "$DOM would not stop; cannot snapshot a running domain safely"
    $VIRSH snapshot-delete "$DOM" --snapshotname "$SNAP" 2>/dev/null || true
    $VIRSH snapshot-create-as "$DOM" --name "$SNAP" \
        --description "clean state, taken after provisioning" --atomic
    log "$DOM snapshot '$SNAP' created"
    ;;
revert)
    $VIRSH snapshot-info "$DOM" --snapshotname "$SNAP" >/dev/null 2>&1 \
        || die "$DOM has no '$SNAP' snapshot; run: snapshot.sh create $DOM"
    $VIRSH destroy "$DOM" 2>/dev/null || true
    $VIRSH snapshot-revert "$DOM" --snapshotname "$SNAP" --force
    log "$DOM reverted to '$SNAP'"
    ;;
*) die "usage: snapshot.sh create|revert <domain>" ;;
esac
