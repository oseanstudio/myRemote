#!/bin/bash
# The app runs this fixed script after macOS authorizes a 30-second AWDL capture.
set -euo pipefail

summarize() {
    /usr/bin/awk -v addresses="$1" '
        BEGIN {
            n = split(tolower(addresses), address, /[[:space:]]+/)
            for (i = 1; i <= n; i++)
                if (address[i] != "") local[address[i]] = 1
        }
        {
            source = ""
            for (i = 2; i <= NF; i++) {
                if ($i == ">") {
                    source = tolower($(i - 1))
                    sub(/\.[0-9]+$/, "", source)
                    sub(/%.*/, "", source)
                    break
                }
            }
            if (source == "") next
            packets++
            message = tolower($0)
            if (message ~ /\?[[:space:]]+_companion-link\._tcp\.local([.[:space:]]|$)/) {
                if (source in local) mac_queries++
                else peer_queries++
            }
            if (source in local && message ~ /[0-9]+\/[0-9]+\/[0-9]+[[:space:]]/ &&
                message ~ /nearby\._companion-link\._tcp\.local([.[:space:]]|$)/) nearby_answers++
        }
        END {
            printf "%d %d %d %d\n", packets, peer_queries, mac_queries, nearby_answers
        }'
}

capture() (
    # Non-promiscuous and mDNS only. Packet text goes straight to the counters.
    /usr/sbin/tcpdump -i awdl0 -p -nn -l -t -vv -s 0 'udp port 5353' &
    capture_pid=$!
    trap 'kill -INT %1 2>/dev/null || :; wait "$capture_pid" || :; exit 1' HUP INT TERM
    for ((second = 0; second < 30; second++)); do
        kill -0 %1 2>/dev/null || break
        /bin/sleep 1
    done
    # A job spec only addresses the tcpdump child tracked by this shell.
    kill -INT %1 2>/dev/null || :
    wait "$capture_pid"
)

if [[ ${1:-} == --summarize && $# == 2 ]]; then
    summarize "$2"
    exit
fi

interface=$(/sbin/ifconfig awdl0) || {
    echo "No address on awdl0" >&2
    exit 1
}
addresses=$(printf '%s\n' "$interface" | /usr/bin/awk '
    $1 == "inet" || $1 == "inet6" {sub(/%.*/, "", $2); print $2}
')
if [[ -z $addresses ]]; then
    echo "No address on awdl0" >&2
    exit 1
fi
capture | summarize "$addresses"
