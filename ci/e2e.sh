#!/usr/bin/env bash
# End-to-end test of a built vmmbox binary, run by the `e2e` job in ci.yml.
#
#   ci/e2e.sh <boot|nohw> <vmmbox binary> <expected guest arch> <accel> [distro]
#
# boot  Needs a working hypervisor. Pulls an image, boots a real VM, runs
#       commands in it, shuts it down through ACPI (the graceful path), boots it
#       again to prove its disk survived, shuts it down again and removes it.
# nohw  For runners that have no hypervisor (GitHub's Arm Linux, macOS and Arm
#       Windows runners expose none). Pulls the image for this architecture and
#       checks that `start` refuses cleanly, with an actionable error, instead of
#       crashing. If it unexpectedly boots, that is reported as a warning: the
#       runner can now do a real boot test.
#
# Everything lives under a scratch VMMBOX_HOME, so nothing on the machine is
# touched and nothing is left behind.

set -euo pipefail

mode="${1:?usage: e2e.sh <boot|nohw> <vmmbox> <arch> <accel> [distro]}"
vmmbox="${2:?missing vmmbox binary}"
want_arch="${3:?missing expected guest architecture}"
accel="${4:?missing accelerator name}"
distro="${5:-ubuntu}"

# Git Bash would rewrite arguments like /etc/os-release into Windows paths.
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

case "$(uname -s)" in
MINGW* | MSYS* | CYGWIN*) windows=1 ;;
*) windows=0 ;;
esac

vmmbox="$(cd "$(dirname "$vmmbox")" && pwd)/$(basename "$vmmbox")"
scratch="${RUNNER_TEMP:-$(mktemp -d)}/vmmbox-e2e"
rm -rf "$scratch"
mkdir -p "$scratch"
# A native Windows program wants C:/... rather than Git Bash's /c/...
if [ "$windows" = 1 ]; then scratch="$(cygpath -m "$scratch")"; fi
export VMMBOX_HOME="$scratch"

step() { printf '\n==> %s\n' "$*"; }
fail() {
	printf '::error::%s\n' "$*"
	exit 1
}
# Run vmmbox with stdin closed, so ssh can never wait on this script's input.
vm() { "$vmmbox" "$@" </dev/null; }
# Same, but also keeps the output (stdout and stderr) on stdout for the caller to
# check, and shows it as it is produced, so a hang can be seen where it happens.
#
# The output goes to a file, never a pipe: `start` leaves QEMU running, and on
# Windows that QEMU inherits whatever handles vmmbox had, so a pipe would not
# reach end-of-file until the VM stopped and this would wait for it.
vmo() {
	local f rc=0 tailer
	f="$(mktemp)"
	tail -f "$f" >&2 &
	tailer=$!
	"$vmmbox" "$@" </dev/null >"$f" 2>&1 || rc=$?
	sleep 1 # let tail print the last lines
	kill "$tailer" 2>/dev/null || true
	wait "$tailer" 2>/dev/null || true
	cat "$f"
	rm -f "$f" || true # QEMU may still hold the file open on Windows
	return "$rc"
}
# The guest answers over ssh; strip the CR a Windows ssh may add.
guest() { vm run "$distro" "$@" | tr -d '\r'; }

qemu_running() {
	if [ "$windows" = 1 ]; then
		local tasks
		tasks="$(tasklist /FI "IMAGENAME eq qemu-system-${want_arch}.exe" /NH 2>/dev/null || true)"
		grep -qi 'qemu-system' <<<"$tasks"
	else
		pgrep -f "qemu-system.*-name vmmbox-$distro" >/dev/null
	fi
}

home_work=""
cleanup() {
	local status=$?
	if [ "$status" -ne 0 ]; then
		touch "$scratch/.diagnosed" # tells the workflow not to repeat this
		printf '\n==== FAILED (exit %s): diagnostics ====\n' "$status"
		"$vmmbox" ps -a 2>&1 || true
		if [ "$windows" = 1 ]; then
			# Which ssh vmmbox would find, and whether its key is private enough.
			printf '\n---- ssh clients on PATH\n'
			where.exe ssh 2>&1 || true
			printf '\n---- permissions on the VM key\n'
			icacls "$scratch/vms/$distro/id_ed25519" 2>&1 || true
		fi
		while IFS= read -r log; do
			printf '\n---- %s (last 60 lines)\n' "$log"
			tail -n 60 "$log" 2>/dev/null || true
		done < <(find "$scratch" -name '*.log' 2>/dev/null)
	fi
	"$vmmbox" rm -f "$distro" </dev/null >/dev/null 2>&1 || true
	[ -n "$home_work" ] && rm -rf "$home_work"
	exit "$status"
}
trap cleanup EXIT

step "vmmbox $("$vmmbox" --version)"

step "pull $distro"
vm pull "$distro"
images="$(vm images)"
printf '%s\n' "$images"
grep -qi "$want_arch" <<<"$images" || fail "the pulled image is not $want_arch"

# ---------------------------------------------------------------------------
if [ "$mode" = nohw ]; then
	# On macOS vmmbox ships its own QEMU: install it from the published,
	# SHA-256-pinned archive and check that it runs on this machine.
	if [ "$(uname -s)" = Darwin ]; then
		step "setup: install and run vmmbox's QEMU"
		vm setup
		qemu_bin="$VMMBOX_HOME/bin/qemu-system-aarch64"
		[ -x "$qemu_bin" ] || fail "the QEMU bundle did not install $qemu_bin"
		"$qemu_bin" --version
	fi

	step "start without a hypervisor must fail cleanly"
	expect="${E2E_EXPECT_ERROR:?nohw mode needs E2E_EXPECT_ERROR}"
	rc=0
	out="$(vmo start "$distro")" || rc=$?
	if [ "$rc" -eq 0 ]; then
		printf '::warning::the %s VM booted on a runner expected to have no hypervisor. Set boot: true for it in ci.yml.\n' "$distro"
		vm stop "$distro"
		exit 0
	fi
	grep -qi 'panicked' <<<"$out" && fail "vmmbox panicked instead of reporting the missing hypervisor"
	grep -Eqi "$expect" <<<"$out" || fail "start failed, but not with a message matching '$expect'"
	echo "OK: start refused with an actionable error"
	exit 0
fi

# ---------------------------------------------------------------------------
step "first boot"
out="$(vmo start "$distro")"
grep -q "Started $distro" <<<"$out" || fail "start did not report the VM as started"
grep -qi "$accel accelerated" <<<"$out" || fail "the VM did not start with $accel acceleration"

step "ps lists it as running"
running="$(vm ps)"
awk -v n="$distro" '$1 == n && $3 == "Up" { found = 1 } END { exit !found }' <<<"$running" ||
	fail "vmmbox ps does not show $distro as Up"
qemu_running || fail "no QEMU process is running"

step "commands run inside the guest"
osrelease="$(guest cat /etc/os-release)"
grep -q "^ID=$distro" <<<"$osrelease" || fail "the guest is not $distro"
arch="$(guest uname -m)"
[ "$arch" = "$want_arch" ] || fail "guest architecture is '$arch', expected $want_arch"
echo "guest: $distro on $arch"

uid="$(guest id -u)"
[ "$uid" != 0 ] || fail "the guest user is root; it should mirror the host user"
if [ "$windows" = 0 ]; then
	[ "$uid" = "$(id -u)" ] || fail "guest uid $uid does not match host uid $(id -u)"
fi

rc=0
vm run "$distro" sh -c 'exit 7' || rc=$?
[ "$rc" = 7 ] || fail "the exit status of a guest command was $rc, expected 7"

if [ "$windows" = 0 ]; then
	step "the host home is shared with the guest, both ways"
	home_work="$HOME/vmmbox-e2e-$$"
	mkdir -p "$home_work"
	echo from-host >"$home_work/host.txt"
	seen="$(cd "$home_work" && guest sh -c 'cat host.txt && echo from-guest >guest.txt')"
	[ "$seen" = from-host ] || fail "the guest could not read a file written on the host (saw '$seen')"
	[ "$(cat "$home_work/guest.txt")" = from-guest ] || fail "the host could not read a file written in the guest"
fi

# Written to the guest's own disk: must survive a shutdown and a second boot.
guest sh -c 'echo persisted | sudo -n tee /var/lib/vmmbox-e2e >/dev/null && sync'

step "graceful shutdown"
out="$(vmo stop "$distro")"
grep -q "$distro stopped" <<<"$out" || fail "stop did not report the VM as stopped"
grep -q 'did not shut down in time' <<<"$out" && fail "the guest ignored the ACPI shutdown and had to be powered off"
running="$(vm ps)"
grep -q 'No running VMs' <<<"$running" || fail "vmmbox ps still lists a running VM"
all="$(vm ps -a)"
awk -v n="$distro" '$1 == n && $3 == "Stopped" { found = 1 } END { exit !found }' <<<"$all" ||
	fail "vmmbox ps -a does not show $distro as Stopped"
! qemu_running || fail "QEMU is still running after stop"

step "second boot: the disk survived the shutdown"
out="$(vmo start "$distro")"
grep -q "Started $distro" <<<"$out" || fail "the VM did not start the second time"
[ "$(guest cat /var/lib/vmmbox-e2e)" = persisted ] || fail "data written before the shutdown is gone"
if [ "$windows" = 0 ]; then
	[ "$(cd "$home_work" && guest cat host.txt)" = from-host ] || fail "the shared home is missing after a restart"
fi

step "graceful shutdown again"
out="$(vmo stop "$distro")"
grep -q 'did not shut down in time' <<<"$out" && fail "the guest ignored the ACPI shutdown the second time"
! qemu_running || fail "QEMU is still running after the second stop"

step "rm removes the VM"
vm rm "$distro"
listed="$(vm ls)"
grep -q 'No VMs' <<<"$listed" || fail "the VM is still listed after rm"

printf '\nOK: booted, ran, shut down gracefully, rebooted and removed %s on %s with %s\n' "$distro" "$want_arch" "$accel"
