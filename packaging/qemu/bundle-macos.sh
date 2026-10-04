#!/usr/bin/env bash
# Make a macOS QEMU install tree self-contained.
#
#   bundle-macos.sh <stage-dir> <hvf-entitlements.plist>
#
# QEMU links Homebrew's dylibs by absolute path. This copies each one (and,
# transitively, theirs) into <stage>/lib, rewrites every reference to be
# relative (@executable_path/../lib for programs, @loader_path for libraries),
# and re-signs everything: rewriting install names invalidates signatures, which
# arm64 refuses to run, and qemu-system needs the Hypervisor entitlement for HVF.
#
# Written for bash 3.2, which is what macOS ships.

set -euo pipefail

die() {
	printf 'bundle-macos.sh: %s\n' "$*" >&2
	exit 1
}

[ $# -eq 2 ] || die "usage: $0 <stage-dir> <entitlements.plist>"
stage="$1"
entitlements="$2"
[ -d "$stage/bin" ] || die "no $stage/bin"
[ -f "$entitlements" ] || die "no such file: $entitlements"
libdir="$stage/lib"
mkdir -p "$libdir"

# Libraries that live outside the OS and must travel with the binaries.
is_external() {
	case "$1" in
	/opt/homebrew/* | /usr/local/* | /opt/local/*) return 0 ;;
	*) return 1 ;;
	esac
}

realpath_of() {
	python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$1"
}

queue=()
for f in "$stage"/bin/*; do
	[ -f "$f" ] && queue+=("$f")
done

i=0
while [ "$i" -lt "${#queue[@]}" ]; do
	file="${queue[$i]}"
	i=$((i + 1))
	case "$file" in
	"$stage"/bin/*) prefix='@executable_path/../lib' ;;
	*) prefix='@loader_path' ;;
	esac
	# `otool -L` lists the file's own install name first for a dylib; that one was
	# already rewritten to a relative name when the library was copied in.
	while IFS= read -r dep; do
		is_external "$dep" || continue
		name="$(basename "$dep")"
		if [ ! -e "$libdir/$name" ]; then
			cp "$(realpath_of "$dep")" "$libdir/$name"
			chmod u+w "$libdir/$name"
			install_name_tool -id "@loader_path/$name" "$libdir/$name" 2>/dev/null
			queue+=("$libdir/$name")
		fi
		install_name_tool -change "$dep" "$prefix/$name" "$file" 2>/dev/null
	done < <(otool -L "$file" | tail -n +2 | awk '{print $1}')
done

# Nothing outside the OS may remain referenced. (Plain ifs: a `cmd && echo` that
# ends on a false test would return 1 and abort a `set -e` script.)
leftover=""
for f in "$stage"/bin/* "$libdir"/*.dylib; do
	[ -f "$f" ] || continue
	while IFS= read -r dep; do
		if is_external "$dep"; then
			leftover="$leftover$f -> $dep"$'\n'
		fi
	done < <(otool -L "$f" | tail -n +2 | awk '{print $1}')
done
if [ -n "$leftover" ]; then
	die "still linked to external libraries:
$leftover"
fi

# Re-sign, libraries first. Ad-hoc: no Developer ID is needed for HVF, and these
# are fetched by vmmbox itself, so they carry no quarantine flag.
for lib in "$libdir"/*.dylib; do
	codesign --force --sign - "$lib" 2>/dev/null
done
for bin in "$stage"/bin/*; do
	[ -f "$bin" ] || continue
	case "$(basename "$bin")" in
	qemu-system-*) codesign --force --sign - --entitlements "$entitlements" "$bin" 2>/dev/null ;;
	*) codesign --force --sign - "$bin" 2>/dev/null ;;
	esac
done

# An invalid signature makes arm64 kill the process with no message at all, so
# verify every file rather than trusting the signing step.
for f in "$stage"/bin/* "$libdir"/*.dylib; do
	[ -f "$f" ] || continue
	codesign --verify --strict "$f" || die "invalid signature: $f"
done
for bin in "$stage"/bin/qemu-system-*; do
	[ -f "$bin" ] || continue
	if ! codesign -d --entitlements - "$bin" 2>/dev/null | grep -q 'com.apple.security.hypervisor'; then
		die "$bin lost the Hypervisor entitlement; HVF would not work"
	fi
done

printf 'bundled %d libraries into %s\n' "$(find "$libdir" -name '*.dylib' | wc -l | tr -d ' ')" "$libdir" >&2
