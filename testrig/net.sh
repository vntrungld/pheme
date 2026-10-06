#!/bin/sh
# Create or destroy the rig's libvirt network.
# Usage: net.sh up | down | status
set -eu
cd "$(dirname "$0")"
. ./rig.conf
. ./lib.sh

VIRSH="virsh -c qemu:///system"

case "${1:-}" in
up)
    if $VIRSH net-info "$RIG_NET" >/dev/null 2>&1; then
        log "network $RIG_NET already defined"
    else
        mkdir -p "$RIG_STATE"
        _gw=$(echo "$RIG_NET_CIDR" | sed 's|0/24|1|')
        cat > "$RIG_STATE/net.xml" <<XML
<network>
  <name>$RIG_NET</name>
  <forward mode='nat'/>
  <bridge stp='on' delay='0'/>
  <ip address='$_gw' netmask='255.255.255.0'>
    <dhcp><range start='$(echo "$RIG_NET_CIDR" | sed 's|0/24|50|')'
                 end='$(echo "$RIG_NET_CIDR" | sed 's|0/24|99|')'/></dhcp>
  </ip>
</network>
XML
        $VIRSH net-define "$RIG_STATE/net.xml"
    fi
    $VIRSH net-start "$RIG_NET" 2>/dev/null || true
    wait_until 30 "network $RIG_NET active" \
        sh -c "$VIRSH net-info $RIG_NET | grep -q 'Active:.*yes'" \
        || die "network $RIG_NET did not become active"
    log "network $RIG_NET up"
    ;;
down)
    $VIRSH net-destroy "$RIG_NET" 2>/dev/null || true
    $VIRSH net-undefine "$RIG_NET" 2>/dev/null || true
    log "network $RIG_NET down"
    ;;
status) $VIRSH net-info "$RIG_NET" ;;
*) die "usage: net.sh up|down|status" ;;
esac
