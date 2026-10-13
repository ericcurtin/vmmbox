#!/usr/bin/env bash
# End-to-end test of a built vmmbox binary, run by the `e2e` job in ci.yml.
#
#   ci/e2e.sh <boot|nohw> <vmmbox binary> <target triple> [distro]
#
# boot  Needs a hypervisor: pulls an image, boots a real VM, runs commands in
#       it, stops it gracefully, boots it again to check its disk survived,
#       stops it again and removes it.
# nohw  For runners with no hypervisor: pulls the image for this architecture
#       and checks that `run` fails with a clear error, not a crash. Warns if
#       the VM boots after all, so the platform can be moved to `boot`.
#
# Everything lives under a scratch VMMBOX_HOME.

set -euo pipefail

mode="${1:?usage: e2e.sh <boot|nohw> <vmmbox> <target triple> [distro]}"
vmmbox="${2:?missing vmmbox binary}"
target="${3:?missing target triple}"
distro="${4:-ubuntu}"

# What the target decides: the accelerator, the error a host without one gives,
# and a start-up failure that is a flake of the runner, not of vmmbox, and is
# retried (WHPX in a Hyper-V VM sometimes panics the guest kernel at boot).
want_arch="${target%%-*}"
windows=0
retry_start_on=""
case "$target" in
*-apple-darwin) accel=hvf nohw_error='Hypervisor\.framework' ;;
*-linux-*) accel=kvm nohw_error='KVM is not usable' ;;
*-windows-*)
	windows=1 accel=whpx nohw_error="qemu-system-$want_arch not found"
	retry_start_on='guest kernel panicked'
	;;
*) echo "unknown target $target" >&2; exit 2 ;;
esac

# Git Bash would rewrite arguments like /etc/os-release into Windows paths.
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1

chmod +x "$vmmbox" # artifacts lose the executable bit
vmmbox="$(cd "$(dirname "$vmmbox")" && pwd)/$(basename "$vmmbox")"
scratch="${RUNNER_TEMP:-$(mktemp -d)}/vmmbox-e2e"
rm -rf "$scratch"
mkdir -p "$scratch"
if [ "$windows" = 1 ]; then scratch="$(cygpath -m "$scratch")"; fi # C:/..., not /c/...
export VMMBOX_HOME="$scratch"

step() { printf '\n==> %s\n' "$*"; }
fail() {
	printf '::error::%s\n' "$*"
	exit 1
}
# vmmbox with stdin closed, so ssh never waits on ours.
vm() { "$vmmbox" "$@" </dev/null; }
# Like `vm`, but shows the output as it is produced and also returns it on
# stdout. It goes through a file, not a pipe: on Windows the QEMU that `run`
# leaves running inherits a pipe's write end, so a pipe would not reach EOF
# until the VM stopped.
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
	rm -f "$f" || true # QEMU may still hold it open on Windows
	return "$rc"
}
guest() { vm run "$distro" "$@" | tr -d '\r'; }

# Start the VM, retrying up to three times if it fails with $retry_start_on.
start_vm() {
	local attempt=1 out rc
	while :; do
		rc=0
		out="$(vmo run "$distro")" || rc=$?
		if [ "$rc" -ne 0 ] && [ "$attempt" -lt 3 ] && [ -n "$retry_start_on" ] &&
			grep -Eq "$retry_start_on" <<<"$out"; then
			attempt=$((attempt + 1))
			printf '::warning::run hit a known flake of this runner (%s): retrying, attempt %s of 3.\n' "$retry_start_on" "$attempt" >&2
			continue
		fi
		printf '%s' "$out"
		return "$rc"
	done
}

qemu_running() {
	if [ "$windows" = 1 ]; then
		local tasks
		tasks="$(tasklist /FI "IMAGENAME eq qemu-system-${want_arch}.exe" /NH 2>/dev/null || true)"
		grep -qi 'qemu-system' <<<"$tasks"
	else
		pgrep -f "qemu-system.*-name vmmbox-$distro" >/dev/null
	fi
}

boot_vm() {
	local out
	out="$(start_vm)"
	grep -q "Started $distro" <<<"$out" || fail "the VM did not start"
	grep -qi "$accel accelerated" <<<"$out" || fail "the VM did not start with $accel acceleration"
	[ -z "${E2E_EXPECT_WARNING:-}" ] || grep -q "$E2E_EXPECT_WARNING" <<<"$out" ||
		fail "run did not warn: '$E2E_EXPECT_WARNING'"
}

stop_vm() {
	local out
	out="$(vmo stop "$distro")"
	grep -q "$distro stopped" <<<"$out" || fail "stop did not report the VM as stopped"
	! grep -q 'did not shut down in time' <<<"$out" || fail "the guest ignored the shutdown request and was powered off"
	! qemu_running || fail "QEMU is still running after stop"
}

# `ps` lists running VMs, `ps -a` all of them.
expect_status() {
	local out
	if [ "$1" = Stopped ]; then
		out="$(vm ps)"
		grep -q 'No running VMs' <<<"$out" || fail "vmmbox ps still lists a running VM"
		out="$(vm ps -a)"
	else
		out="$(vm ps)"
	fi
	awk -v n="$distro" -v s="$1" '$1 == n && $3 == s { found = 1 } END { exit !found }' <<<"$out" ||
		fail "vmmbox ps does not show $distro as $1"
}

home_work=""
cleanup() {
	local status=$?
	if [ "$status" -ne 0 ]; then
		touch "$scratch/.diagnosed" # tells the workflow not to repeat this
		printf '\n==== FAILED (exit %s): diagnostics ====\n' "$status"
		"$vmmbox" ps -a 2>&1 || true
		while IFS= read -r log; do
			printf '\n---- %s (last 60 lines)\n' "$log"
			tail -n 60 "$log" 2>/dev/null || true
		done < <(find "$scratch" -name '*.log' 2>/dev/null)
		# `rm -f` below deletes the VM's logs: keep whole copies for the artifact.
		mkdir -p "$scratch/kept-logs"
		while IFS= read -r log; do
			cp "$log" "$scratch/kept-logs/$(basename "$(dirname "$log")")-$(basename "$log")" 2>/dev/null || true
		done < <(find "$scratch/vms" -name '*.log' 2>/dev/null)
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

if [ "$mode" = nohw ]; then
	# macOS: also install vmmbox's own pinned QEMU and check that it runs here.
	if [ "$accel" = hvf ]; then
		step "setup: install and run vmmbox's QEMU"
		vm setup
		"$VMMBOX_HOME/bin/qemu-system-aarch64" --version || fail "the QEMU bundle does not run"
	fi

	step "run without a hypervisor must fail cleanly"
	rc=0
	out="$(vmo run "$distro")" || rc=$?
	if [ "$rc" -eq 0 ]; then
		printf '::warning::the %s VM booted on a runner expected to have no hypervisor: set mode: boot for it in ci.yml.\n' "$distro"
		vm stop "$distro" || printf '::warning::could not stop it cleanly.\n'
		exit 0
	fi
	! grep -qi 'panicked' <<<"$out" || fail "vmmbox panicked instead of reporting the missing hypervisor"
	grep -Eqi "$nohw_error" <<<"$out" || fail "run failed, but not with a message matching '$nohw_error'"
	echo "OK: run refused with an actionable error"
	exit 0
fi

step "first boot"
boot_vm
expect_status Up
qemu_running || fail "no QEMU process is running"

step "commands run inside the guest"
osrelease="$(guest cat /etc/os-release)"
grep -q "^ID=$distro" <<<"$osrelease" || fail "the guest is not $distro"
arch="$(guest uname -m)"
[ "$arch" = "$want_arch" ] || fail "guest architecture is '$arch', expected $want_arch"
uid="$(guest id -u)"
[ "$uid" != 0 ] || fail "the guest user is root; it should mirror the host user"
[ "$windows" = 1 ] || [ "$uid" = "$(id -u)" ] || fail "guest uid $uid does not match host uid $(id -u)"
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

# On the guest's own disk: must survive a shutdown and a second boot.
guest sh -c 'echo persisted | sudo -n tee /var/lib/vmmbox-e2e >/dev/null && sync'

step "graceful shutdown"
stop_vm
expect_status Stopped

step "second boot: the disk survived"
boot_vm
[ "$(guest cat /var/lib/vmmbox-e2e)" = persisted ] || fail "data written before the shutdown is gone"
[ "$windows" = 1 ] || [ "$(cd "$home_work" && guest cat host.txt)" = from-host ] ||
	fail "the shared home is missing after a restart"

step "graceful shutdown again"
stop_vm

# A sound setup QEMU cannot use must not stop the VM or the home share. QEMU
# connects to the virtio-fs server before it sets up sound, so the retry needs a
# new server. (Windows has no share, and its runner has no sound device anyway.)
if [ "$windows" = 0 ]; then
	step "an unusable sound backend: the VM starts without sound, home still shared"
	out="$(VMMBOX_AUDIO=bogus vmo run "$distro")"
	grep -q 'could not start with sound' <<<"$out" || fail "run did not fall back from the broken sound backend"
	grep -q "Started $distro" <<<"$out" || fail "the VM did not start without sound"
	[ "$(cd "$home_work" && guest cat host.txt)" = from-host ] || fail "the home share is broken after the fallback"
	stop_vm
fi

step "rm removes the VM"
vm rm "$distro"
listed="$(vm ls)"
grep -q 'No VMs' <<<"$listed" || fail "the VM is still listed after rm"

printf '\nOK: booted, ran, stopped gracefully, rebooted and removed %s on %s with %s\n' "$distro" "$want_arch" "$accel"
