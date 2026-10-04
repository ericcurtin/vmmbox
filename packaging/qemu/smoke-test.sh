#!/usr/bin/env bash
# Check that a QEMU install tree has what vmmbox needs.
#
#   smoke-test.sh <root> <target>
#
# Runs against the tree itself, so it proves the bundle is self-contained, not
# just that the build host happened to have the right libraries installed.

set -euo pipefail

die() {
	printf 'smoke-test: FAIL: %s\n' "$*" >&2
	exit 1
}
ok() { printf 'smoke-test: ok: %s\n' "$*" >&2; }

[ $# -eq 2 ] || die "usage: $0 <root> <target>"
root="$1"
target="$2"

HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=VERSION
. "$HERE/VERSION"
# For checking the check itself on a machine newer than the floor.
MIN_MACOS="${SMOKE_MIN_MACOS:-$MIN_MACOS}"

case "$target" in
aarch64-apple-darwin)
	arch=aarch64 accel=hvf audio=coreaudio machine=virt firmware="edk2-aarch64-code.fd edk2-arm-vars.fd" ;;
*) die "unknown target: $target" ;;
esac

qemu="$root/bin/qemu-system-$arch"
img="$root/bin/qemu-img"
[ -x "$qemu" ] || die "missing $qemu"
[ -x "$img" ] || die "missing $img"

out="$("$qemu" --version | head -n1)" || die "qemu-system does not run"
case "$out" in
*"$QEMU_VERSION"*) ok "$out" ;;
*) die "expected version $QEMU_VERSION, got: $out" ;;
esac

"$qemu" -accel help | grep -qx "$accel" || die "no $accel accelerator"
ok "accelerator $accel"

devices="$("$qemu" -device help 2>&1)"
for d in vhost-user-fs-pci virtio-sound-pci virtio-blk-pci virtio-net-pci virtio-rng-pci; do
	printf '%s\n' "$devices" | grep -q "name \"$d\"" || die "missing device $d"
done
ok "devices: virtio-fs, sound, blk, net, rng"

# virtio-fs shares guest RAM with the file server, which on macOS needs the
# POSIX-shm memory backend (there is no memfd).
"$qemu" -object help 2>&1 | grep -q "memory-backend-shm" || die "no memory-backend-shm"
ok "shared-memory backend"

"$qemu" -audiodev help 2>&1 | grep -qx "$audio" || die "no $audio audio driver"
ok "audio driver $audio"

"$qemu" -machine help | grep -q "^$machine " || die "no $machine machine"
ok "machine $machine"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
"$img" create -f qcow2 -o compression_type=zstd "$tmp/t.qcow2" 1G >/dev/null || die "qcow2 with zstd compression unsupported"
ok "qcow2 + zstd"

for f in $firmware; do
	[ -f "$root/share/qemu/$f" ] || die "missing firmware share/qemu/$f"
done
[ -f "$root/share/qemu/efi-virtio.rom" ] || die "missing share/qemu/efi-virtio.rom (virtio-net loads it by default)"
ok "firmware and ROMs"

# QEMU must find its firmware relative to itself, not in the build prefix.
"$qemu" -L help 2>&1 | grep -q "$root/bin/../share/qemu" || "$qemu" -L help 2>&1 | grep -q "$root/share/qemu" ||
	die "QEMU does not search the bundle's share/qemu"
ok "relocatable data directory"

if [ "$(uname -s)" = Darwin ]; then
	bad="$(otool -L "$qemu" "$img" "$root"/lib/*.dylib 2>/dev/null | awk '{print $1}' | grep -E '^(/opt/homebrew|/usr/local|/opt/local)/' || true)"
	[ -z "$bad" ] || die "links outside the OS and the bundle: $bad"
	# The floor in VERSION must be true: nothing in the bundle may need a newer macOS.
	newest=0
	for f in "$qemu" "$img" "$root"/lib/*.dylib; do
		v="$(otool -l "$f" | awk '/LC_BUILD_VERSION/ {b=1} b && /minos/ {print $2; exit}')"
		major="${v%%.*}"
		[ -z "$major" ] || [ "$major" -le "$newest" ] || newest="$major"
	done
	[ "$newest" -le "$MIN_MACOS" ] || die "the bundle needs macOS $newest, but VERSION says $MIN_MACOS"
	ok "needs macOS $newest or newer (VERSION says $MIN_MACOS)"
	codesign --verify --strict "$qemu" || die "invalid signature on qemu-system"
	codesign -d --entitlements - "$qemu" 2>/dev/null | grep -q com.apple.security.hypervisor || die "no Hypervisor entitlement"
	ok "self-contained, signed, HVF entitlement"
fi
printf 'smoke-test: all checks passed\n' >&2
