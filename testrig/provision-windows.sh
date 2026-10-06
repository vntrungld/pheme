#!/bin/sh
# Build the Windows 11 VM from the Enterprise Evaluation ISO.
#
# UEFI and an emulated TPM 2.0 are not options: Windows 11 Setup refuses a
# machine without them, and this rig uses the supported path rather than a
# registry bypass. Both `swtpm` and OVMF were confirmed present on the
# development machine before this design was written.
#
# The answer file and the setup script ride in on a second, tiny ISO built
# with xorriso. Windows Setup looks for autounattend.xml on removable media
# automatically.
set -eu
cd "$(dirname "$0")"
. ./rig.conf
. ./lib.sh

VIRSH="virsh -c qemu:///system"
ISO="${WINDOWS_ISO:-}"
DISK="$RIG_STATE/$WINDOWS_VM.qcow2"
SEED="$RIG_STATE/windows-seed.iso"

if [ "${1:-}" = "--rebuild" ]; then
    $VIRSH destroy "$WINDOWS_VM" 2>/dev/null || true
    $VIRSH undefine "$WINDOWS_VM" --nvram --remove-all-storage 2>/dev/null || true
fi
if $VIRSH dominfo "$WINDOWS_VM" >/dev/null 2>&1; then
    log "$WINDOWS_VM already exists; pass --rebuild to replace it"
    exit 0
fi

[ -n "$ISO" ] || die "set WINDOWS_ISO to the Windows 11 Enterprise Evaluation ISO
  Get it from: https://www.microsoft.com/en-us/evalcenter/download-windows-11-enterprise
  Then: WINDOWS_ISO=/path/to/it.iso sh testrig/provision-windows.sh"
[ -f "$ISO" ] || die "no such file: $ISO"

mkdir -p "$RIG_STATE"
[ -f "$RIG_SSH_KEY" ] || ssh-keygen -t ed25519 -N '' -C pheme-rig -f "$RIG_SSH_KEY"

# The seed ISO: answer file, setup script, and the public key it installs.
rm -rf "$RIG_STATE/seed" && mkdir -p "$RIG_STATE/seed"
cp windows/autounattend.xml windows/setup-ssh.ps1 "$RIG_STATE/seed/"
cp "$RIG_SSH_KEY.pub" "$RIG_STATE/seed/id_rig.pub"
xorriso -as mkisofs -J -r -V RIGSEED -o "$SEED" "$RIG_STATE/seed"

qemu-img create -f qcow2 "$DISK" "${WINDOWS_DISK_GB}G"

log "creating $WINDOWS_VM (Windows Setup takes 20-40 minutes unattended)"
virt-install \
  --connect qemu:///system \
  --name "$WINDOWS_VM" \
  --memory "$WINDOWS_RAM_MB" --vcpus 4 \
  --cpu host-passthrough \
  --disk "path=$DISK,format=qcow2,bus=sata" \
  --cdrom "$ISO" \
  --disk "path=$SEED,device=cdrom" \
  --os-variant win11 \
  --network "network=$RIG_NET,model=e1000e" \
  --graphics spice --video qxl \
  --boot uefi \
  --tpm backend.type=emulator,backend.version=2.0,model=tpm-crb \
  --noautoconsole

# Under UEFI the Windows ISO's boot loader waits a few seconds for "Press any
# key to boot from CD or DVD" and, unanswered, falls through to the empty
# disk. Keep pressing Enter through that window: the condition never comes
# true, so this is wait_until used as a bounded repeat, and its timeout is the
# expected outcome. Setup ignores the keys once it is running unattended.
wait_until 30 "the CD boot prompt to be answered" \
    sh -c "$VIRSH send-key $WINDOWS_VM KEY_ENTER >/dev/null 2>&1; false" || true

# WinPE may not take a lease at all; the installed system does, after the
# first reboot, which is minutes into Setup.
wait_until 1800 "$WINDOWS_VM to get a DHCP lease" rig_vm_ip "$WINDOWS_VM" \
    || die "$WINDOWS_VM never got an address"
IP=$(rig_vm_ip "$WINDOWS_VM")
log "$WINDOWS_VM is at $IP"

# The marker is written in Setup's specialize pass, before anyone logs on, so
# a running explorer is also required: provisioning is done when rig's desktop
# exists, not when sshd answers.
wait_until 3600 "$WINDOWS_VM to finish Setup and log rig on" \
    rig_ssh "$IP" rig 'if ((Test-Path C:\Users\Public\pheme-rig-ready) -and (Get-Process -Name explorer -ErrorAction SilentlyContinue)) { exit 0 } else { exit 1 }' \
    || die "$WINDOWS_VM never finished Setup; open its console: virt-viewer $WINDOWS_VM (setup log: C:\Windows\Temp\pheme-rig-setup.log)"

log "$WINDOWS_VM provisioned"
