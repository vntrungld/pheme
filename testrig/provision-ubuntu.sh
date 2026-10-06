#!/bin/sh
# Build the Ubuntu 24.04 Desktop VM. Idempotent: an existing domain is left
# alone unless --rebuild is passed.
#
# Built from the cloud image plus cloud-init rather than the desktop ISO with
# autoinstall: cloud-init is a documented unattended path, whereas driving the
# desktop installer means screen-scraping. `apt install ubuntu-desktop` on top
# gives the same GNOME-on-Wayland session a person would have, and an X11
# session is selectable from the same VM for the W rows.
set -eu
cd "$(dirname "$0")"
. ./rig.conf
. ./lib.sh

VIRSH="virsh -c qemu:///system"
IMG_URL=https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img
BASE="$RIG_STATE/ubuntu-base.img"
DISK="$RIG_STATE/$UBUNTU_VM.qcow2"

if [ "${1:-}" = "--rebuild" ]; then
    $VIRSH destroy "$UBUNTU_VM" 2>/dev/null || true
    $VIRSH undefine "$UBUNTU_VM" --nvram --remove-all-storage 2>/dev/null || true
fi
if $VIRSH dominfo "$UBUNTU_VM" >/dev/null 2>&1; then
    log "$UBUNTU_VM already exists; pass --rebuild to replace it"
    exit 0
fi

mkdir -p "$RIG_STATE"
[ -f "$RIG_SSH_KEY" ] || ssh-keygen -t ed25519 -N '' -C pheme-rig -f "$RIG_SSH_KEY"
[ -f "$BASE" ] || { log "downloading the Ubuntu cloud image"; curl -fL -o "$BASE" "$IMG_URL"; }

cp "$BASE" "$DISK"
qemu-img resize "$DISK" "${UBUNTU_DISK_GB}G"

cat > "$RIG_STATE/ubuntu-user-data" <<YAML
#cloud-config
hostname: pheme-ubuntu
users:
  - name: rig
    groups: [sudo, input]
    shell: /bin/bash
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    ssh_authorized_keys:
      - $(cat "$RIG_SSH_KEY.pub")
package_update: true
packages: [ubuntu-desktop-minimal, pipewire, pipewire-pulse, wireplumber]
runcmd:
  # Autologin: pheme's input capture needs a live graphical session, and a VM
  # parked at a login screen has none. Without this, product rows fail for an
  # infrastructure reason.
  - |
    install -d /etc/gdm3
    printf '[daemon]\nAutomaticLoginEnable=true\nAutomaticLogin=rig\n' \
      > /etc/gdm3/custom.conf
  - systemctl set-default graphical.target
  # A marker the runner polls for. Written last, so its presence means every
  # step above finished -- cloud-init's own status is not enough, because the
  # desktop packages are what the rows need and they install late.
  - touch /var/lib/cloud/pheme-rig-ready
YAML

log "creating $UBUNTU_VM (installing the desktop takes several minutes)"
virt-install \
  --connect qemu:///system \
  --name "$UBUNTU_VM" \
  --memory "$UBUNTU_RAM_MB" --vcpus 2 \
  --disk "path=$DISK,format=qcow2" \
  --os-variant ubuntu24.04 \
  --network "network=$RIG_NET,model=virtio" \
  --graphics spice --video virtio \
  --cloud-init "user-data=$RIG_STATE/ubuntu-user-data" \
  --import --noautoconsole

wait_until 120 "$UBUNTU_VM to get a DHCP lease" rig_vm_ip "$UBUNTU_VM" \
    || die "$UBUNTU_VM never got an address"
IP=$(rig_vm_ip "$UBUNTU_VM")
log "$UBUNTU_VM is at $IP"

wait_until 1800 "$UBUNTU_VM cloud-init to finish the desktop install" \
    rig_ssh "$IP" rig test -f /var/lib/cloud/pheme-rig-ready \
    || die "$UBUNTU_VM never finished provisioning; check: rig_ssh $IP rig sudo cloud-init status --long"

log "$UBUNTU_VM provisioned"
