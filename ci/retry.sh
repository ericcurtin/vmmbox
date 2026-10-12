#!/usr/bin/env bash
# Run a command, trying up to three times. The package installs in CI download
# from servers that now and then refuse a connection.
for attempt in 1 2 3; do
	"$@" && exit 0
	if [ "$attempt" -lt 3 ]; then
		printf '::warning::%s failed (attempt %s of 3), trying again\n' "$*" "$attempt" >&2
		sleep $((attempt * 20))
	fi
done
exit 1
