#!/usr/bin/env bash
# Render the Homebrew formula and Scoop manifest for one vmmbox release.
# Used by the release workflow; standalone so it can be run locally:
#
#   packaging/render.sh --version 0.1.0 --checksums checksums.txt --out-dir /tmp/out
#
# packaging/<pkg>/ mirrors the repo it is published to: <out>/homebrew is the
# layout of ericcurtin/homebrew-tap and <out>/scoop that of
# ericcurtin/scoop-bucket. Files ending in .in are rendered; others are copied.
#
# Hashes come from the release's own checksums.txt and are never recomputed,
# so the packages and the published binaries cannot disagree.

set -euo pipefail

die() {
	printf 'render.sh: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat >&2 <<-EOF
		usage: render.sh --version <x.y.z> --checksums <file> --out-dir <dir> [--repo <owner/repo>]

		  --version    release version (the tag without its leading "v")
		  --checksums  sha256sum-format file covering the release's assets
		  --out-dir    directory to write homebrew/ and scoop/ into
		  --repo       GitHub repo the download URLs point at (default: ericcurtin/vmmbox)
	EOF
	exit 2
}

VERSION=""
CHECKSUMS=""
OUT_DIR=""
REPO="ericcurtin/vmmbox"

while [ $# -gt 0 ]; do
	case "$1" in
	--version) VERSION="${2:-}"; shift 2 ;;
	--checksums) CHECKSUMS="${2:-}"; shift 2 ;;
	--out-dir) OUT_DIR="${2:-}"; shift 2 ;;
	--repo) REPO="${2:-}"; shift 2 ;;
	-h | --help) usage ;;
	*) die "unknown argument: $1" ;;
	esac
done

if [ -z "$VERSION" ] || [ -z "$CHECKSUMS" ] || [ -z "$OUT_DIR" ]; then usage; fi
[ -f "$CHECKSUMS" ] || die "no such checksums file: $CHECKSUMS"

# Strictly MAJOR.MINOR.PATCH: it lands in Ruby, JSON and a URL, and is what
# Homebrew and Scoop sort by.
case "$VERSION" in
*[!0-9.]* | . | *..* | .* | *.) die "version \"$VERSION\" is not MAJOR.MINOR.PATCH" ;;
esac
dots="${VERSION//[!.]/}"
[ "${#dots}" -eq 2 ] || die "version \"$VERSION\" is not MAJOR.MINOR.PATCH"

case "$REPO" in
*[!A-Za-z0-9._/-]* | */*/* | "" | /* | */) die "repo \"$REPO\" is not owner/name" ;;
esac

PACKAGING_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(dirname -- "$PACKAGING_DIR")"
TAG="v$VERSION"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

# One description for the crate, the formula and the Scoop manifest.
DESCRIPTION="$(awk '
	/^\[package\]/ { in_pkg = 1; next }
	/^\[/          { in_pkg = 0 }
	in_pkg && /^[[:space:]]*description[[:space:]]*=/ {
		gsub(/^[^"]*"|".*$/, "")
		print
		exit
	}
' "$REPO_ROOT/Cargo.toml")"
[ -n "$DESCRIPTION" ] || die "could not read [package] description from $REPO_ROOT/Cargo.toml"
case "$DESCRIPTION" in
*[\"\\\|@]*) die "description contains a character that cannot be interpolated safely: $DESCRIPTION" ;;
esac

sha_for() {
	local asset="$1" hash
	# Tolerates sha256sum's "*" binary marker and a directory prefix.
	hash="$(awk -v want="$asset" '{ n = $NF; sub(/^\*/, "", n); sub(/.*\//, "", n); if (n == want) { print $1; exit } }' "$CHECKSUMS")"
	[ -n "$hash" ] || die "no sha256 for \"$asset\" in $CHECKSUMS"
	[ "${#hash}" -eq 64 ] || die "malformed sha256 for \"$asset\": \"$hash\""
	case "$hash" in
	*[!0-9a-f]*) die "malformed sha256 for \"$asset\": \"$hash\"" ;;
	esac
	printf '%s' "$hash"
}

SHA_MACOS_ARM64="$(sha_for vmmbox-aarch64-apple-darwin)"
SHA_LINUX_X86_64="$(sha_for vmmbox-x86_64-unknown-linux-musl)"
SHA_WINDOWS_X86_64="$(sha_for vmmbox-x86_64-pc-windows-msvc.exe)"

# `|` as the sed delimiter since several values are URLs; no value can contain
# a placeholder (the description is checked above).
render() {
	sed \
		-e "s|@VERSION@|$VERSION|g" \
		-e "s|@TAG@|$TAG|g" \
		-e "s|@REPO@|$REPO|g" \
		-e "s|@DESCRIPTION@|$DESCRIPTION|g" \
		-e "s|@BASE_URL@|$BASE_URL|g" \
		-e "s|@SHA_MACOS_ARM64@|$SHA_MACOS_ARM64|g" \
		-e "s|@SHA_LINUX_X86_64@|$SHA_LINUX_X86_64|g" \
		-e "s|@SHA_WINDOWS_X86_64@|$SHA_WINDOWS_X86_64|g" \
		"$1"
}

printf 'rendered vmmbox %s (tag %s)\n' "$VERSION" "$TAG" >&2
for pkg in homebrew scoop; do
	while IFS= read -r src; do
		dest="$OUT_DIR/${src#"$PACKAGING_DIR/"}"
		mkdir -p "$(dirname -- "$dest")"
		case "$src" in
		*.in)
			dest="${dest%.in}"
			render "$src" >"$dest"
			# A leftover @PLACEHOLDER@ is a template/script mismatch; never publish it.
			if grep -n '@[A-Z_]\{2,\}@' "$dest"; then
				die "unsubstituted placeholder(s) left in $dest (see above)"
			fi
			;;
		*) cp "$src" "$dest" ;;
		esac
		printf '  %s\n' "$dest" >&2
	done < <(find "$PACKAGING_DIR/$pkg" -type f | sort)
done
