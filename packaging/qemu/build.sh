#!/usr/bin/env bash
# Build vmmbox's QEMU bundle from the unmodified upstream release tarball.
#
#   packaging/qemu/build.sh <target> [--work DIR] [--out DIR]
#
#   target   aarch64-apple-darwin or x86_64-apple-darwin
#   --work   scratch directory (default: ./build/qemu)
#   --out    where the .tar.gz and its .sha256 go (default: ./dist)
#
# No patches and no fork: the inputs are the release tarball pinned in VERSION
# and the configure flags below. `--without-default-features` keeps the result
# independent of whatever happens to be installed on the build machine; only the
# features vmmbox uses are switched on.
#
# The archive unpacks into a prefix: bin/, lib/ and share/qemu/ side by side,
# which is the layout QEMU's relocatable lookups expect (firmware in
# ../share/qemu, libraries via @executable_path/../lib).

set -euo pipefail

die() {
	printf 'build.sh: %s\n' "$*" >&2
	exit 1
}
step() { printf '\n==> %s\n' "$*" >&2; }

HERE="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=VERSION
. "$HERE/VERSION"

TARGET=""
WORK="$PWD/build/qemu"
OUT="$PWD/dist"
while [ $# -gt 0 ]; do
	case "$1" in
	--work) WORK="${2:?}"; shift 2 ;;
	--out) OUT="${2:?}"; shift 2 ;;
	-h | --help) sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
	-*) die "unknown option: $1" ;;
	*) [ -z "$TARGET" ] || die "one target only"; TARGET="$1"; shift ;;
	esac
done
[ -n "$TARGET" ] || die "usage: $0 <target> (see --help)"

# Per-target settings. KEEP lists the only files kept from share/qemu: the
# firmware for the architecture vmmbox runs, plus the network boot ROMs QEMU
# loads by default for virtio-net.
case "$TARGET" in
aarch64-apple-darwin)
	SYSTEM_TARGET=aarch64-softmmu
	SYSTEM_BIN=qemu-system-aarch64
	FLAVOUR=macos
	# vhost-user is off by default outside Linux, but upstream builds it fine
	# on macOS. vmmbox needs it for virtio-fs: the host-side file server is a
	# separate process that QEMU reaches over a vhost-user socket.
	CONFIGURE_FLAGS=(--enable-tcg --enable-hvf --enable-coreaudio --audio-drv-list=coreaudio --enable-vhost-user)
	KEEP_FIRMWARE=(edk2-aarch64-code.fd edk2-arm-vars.fd edk2-licenses.txt)
	;;
x86_64-apple-darwin)
	SYSTEM_TARGET=x86_64-softmmu
	SYSTEM_BIN=qemu-system-x86_64
	FLAVOUR=macos
	CONFIGURE_FLAGS=(--enable-tcg --enable-hvf --enable-coreaudio --audio-drv-list=coreaudio --enable-vhost-user)
	# SeaBIOS and what it loads; the q35 machine vmmbox uses has no UEFI.
	KEEP_FIRMWARE=(bios-256k.bin kvmvapic.bin linuxboot_dma.bin)
	;;
*) die "unsupported target: $TARGET" ;;
esac

case "$FLAVOUR" in
macos) [ "$(uname -s)" = Darwin ] || die "$TARGET must be built on macOS" ;;
esac

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
jobs_count() {
	if [ "$FLAVOUR" = macos ]; then sysctl -n hw.logicalcpu; else nproc; fi
}

mkdir -p "$WORK" "$OUT"
WORK="$(CDPATH='' cd -- "$WORK" && pwd)"
OUT="$(CDPATH='' cd -- "$OUT" && pwd)"
SRC="$WORK/src"
STAGE="$WORK/stage"

# ---------------------------------------------------------------- fetch + verify
step "Fetching QEMU $QEMU_VERSION"
TARBALL="$WORK/qemu-$QEMU_VERSION.tar.xz"
[ -f "$TARBALL" ] || curl -fsSL -o "$TARBALL" "$QEMU_URL"
got="$(sha256_of "$TARBALL")"
[ "$got" = "$QEMU_SHA256" ] || die "checksum mismatch for $TARBALL: expected $QEMU_SHA256, got $got"
echo "sha256 verified: $got" >&2

rm -rf "${SRC:?}" "${STAGE:?}"
mkdir -p "$SRC"
tar -xf "$TARBALL" -C "$SRC" --strip-components=1

# ---------------------------------------------------------------- python deps
# QEMU's configure builds a venv and wants two small pure-Python packages (a
# test-output formatter and its own QMP client). Use them if present, else make
# a venv with pinned versions.
PYTHON="${PYTHON:-python3}"
if ! "$PYTHON" -c 'import pycotap, importlib.metadata as m; m.version("qemu.qmp")' >/dev/null 2>&1; then
	step "Installing QEMU's Python build dependencies into a venv"
	"$PYTHON" -m venv "$WORK/venv"
	"$WORK/venv/bin/pip" install --quiet pycotap==1.3.1 qemu.qmp==0.0.6
	PYTHON="$WORK/venv/bin/python3"
fi

# ---------------------------------------------------------------- configure + build
step "Configuring ($TARGET)"
if [ "$FLAVOUR" = macos ]; then
	brew_prefix="$(brew --prefix)"
	export PKG_CONFIG_PATH="${PKG_CONFIG_PATH:-}:$brew_prefix/lib/pkgconfig:$brew_prefix/share/pkgconfig"
fi
(
	cd "$SRC"
	./configure \
		--python="$PYTHON" \
		--prefix="$STAGE" \
		--target-list="$SYSTEM_TARGET" \
		--without-default-features \
		--enable-slirp --enable-virtfs --enable-tools --enable-zstd --enable-pixman \
		--enable-fdt=internal \
		"${CONFIGURE_FLAGS[@]}"
	step "Building"
	make -j"$(jobs_count)"
	step "Installing"
	make install
)

# ---------------------------------------------------------------- prune
step "Pruning to what vmmbox uses"
find "$STAGE/bin" -type f ! -name "$SYSTEM_BIN" ! -name 'qemu-img' -delete
rm -rf "${STAGE:?}/include" "${STAGE:?}/lib" "${STAGE:?}/share/applications" "${STAGE:?}/share/icons"
mkdir -p "$STAGE/share/qemu.keep"
for f in "${KEEP_FIRMWARE[@]}"; do cp "$STAGE/share/qemu/$f" "$STAGE/share/qemu.keep/"; done
cp "$STAGE"/share/qemu/efi-*.rom "$STAGE/share/qemu.keep/"
rm -rf "${STAGE:?}/share/qemu"
mv "$STAGE/share/qemu.keep" "$STAGE/share/qemu"

# ---------------------------------------------------------------- bundle libraries
if [ "$FLAVOUR" = macos ]; then
	step "Bundling libraries and signing"
	"$HERE/bundle-macos.sh" "$STAGE" "$SRC/accel/hvf/entitlements.plist"
fi

# ---------------------------------------------------------------- notices
step "Writing licence notices"
cp "$SRC/COPYING" "$STAGE/COPYING"
cp "$SRC/COPYING.LIB" "$STAGE/COPYING.LIB"
BUILT_FROM="${GITHUB_SHA:-$(git -C "$HERE" rev-parse HEAD 2>/dev/null || echo unknown)}"
{
	cat <<NOTICE
vmmbox QEMU $QEMU_VERSION (bundle revision $BUNDLE_REV) for $TARGET

This is UNMODIFIED upstream QEMU, built from the release tarball
  $QEMU_URL
  sha256 $QEMU_SHA256
with the configure flags in packaging/qemu/build.sh of
  https://github.com/ericcurtin/vmmbox (commit $BUILT_FROM).
No patches are applied. That tarball is the complete corresponding source; to
rebuild this bundle, run packaging/qemu/build.sh $TARGET at that commit.

QEMU is licensed under the GNU General Public License, version 2 (COPYING), with
parts under other compatible licences; see the source tree. COPYING.LIB is the
GNU Lesser General Public License that covers parts of it.

NOTICE
	if [ "$FLAVOUR" = macos ]; then
		cat <<NOTICE

Bundled libraries (lib/), dynamically linked and replaceable: copied unmodified
from Homebrew, which builds them from the upstream sources named here.
NOTICE
		for f in glib gettext pcre2 libslirp pixman zstd; do
			printf '  %-9s %s\n' "$f" "$(brew list --versions "$f" | awk '{print $2}')"
		done
		cat <<'NOTICE'

  glib      LGPL-2.1-or-later   https://download.gnome.org/sources/glib/
  gettext   LGPL-2.1-or-later   https://ftp.gnu.org/gnu/gettext/   (libintl)
  pcre2     BSD-3-Clause        https://github.com/PCRE2Project/pcre2
  libslirp  BSD-3-Clause        https://gitlab.freedesktop.org/slirp/libslirp
  pixman    MIT                 https://www.pixman.org/
  zstd      BSD-3-Clause        https://github.com/facebook/zstd

Each library's licence text is in licenses/.
NOTICE
	fi
} >"$STAGE/NOTICE.txt"
if [ "$FLAVOUR" = macos ]; then
	mkdir -p "$STAGE/licenses"
	for f in glib gettext pcre2 libslirp pixman zstd; do
		prefix="$(brew --prefix "$f")"
		mkdir -p "$STAGE/licenses/$f"
		for lic in "$prefix"/COPYING* "$prefix"/LICENSE* "$prefix"/LICENCE*; do
			[ -f "$lic" ] && cp "$lic" "$STAGE/licenses/$f/"
		done
	done
fi

# ---------------------------------------------------------------- verify + archive
step "Smoke test"
"$HERE/smoke-test.sh" "$STAGE" "$TARGET"

step "Archiving"
ARCHIVE="vmmbox-qemu-$QEMU_VERSION-r$BUNDLE_REV-$TARGET.tar.gz"
# COPYFILE_DISABLE: do not let macOS tar add AppleDouble ._ files.
(cd "$STAGE" && COPYFILE_DISABLE=1 tar -czf "$OUT/$ARCHIVE" .)
(cd "$OUT" && printf '%s  %s\n' "$(sha256_of "$ARCHIVE")" "$ARCHIVE" >"$ARCHIVE.sha256")
printf '\n%s\n' "$OUT/$ARCHIVE" >&2
cat "$OUT/$ARCHIVE.sha256" >&2
