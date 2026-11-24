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
    duration=$1
    shift
    "$@" &
    capture_pid=$!
    trap 'kill -KILL %1 2>/dev/null || :; exit 1' HUP INT TERM
    for ((second = 0; second < duration; second++)); do
        if ! kill -0 %1 2>/dev/null; then
            wait "$capture_pid"
            return
        fi
        /bin/sleep 1
    done

    # TERM lets tcpdump flush normally. A blocked capture gets one second before
    # KILL. The job spec addresses only the child tracked by this shell.
    if kill -TERM %1 2>/dev/null; then
        /bin/sleep 1
        kill -KILL %1 2>/dev/null || :
        status=0
        wait "$capture_pid" || status=$?
        case "$status" in
            0|137|143) return 0 ;;
            *) return "$status" ;;
        esac
    fi
    wait "$capture_pid"
)

diagnostic() {
    interface=$(/sbin/ifconfig awdl0) || {
        echo "No address on awdl0" >&2
        return 1
    }
    addresses=$(printf '%s\n' "$interface" | /usr/bin/awk '
        $1 == "inet" || $1 == "inet6" {sub(/%.*/, "", $2); print $2}
    ')
    if [[ -z $addresses ]]; then
        echo "No address on awdl0" >&2
        return 1
    fi
    # Non-promiscuous and mDNS only. Packet text goes straight to the counters.
    capture 30 /usr/sbin/tcpdump -i awdl0 --immediate-mode -p -nn -l -t -vv -s 0 'udp port 5353' |
        summarize "$addresses"
}
