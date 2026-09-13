#!/bin/sh
# mk-whodial <tcp|udp> <address> <port>
#
# Which process in this guest holds the socket connected to <address>:<port>?
# Prints {"found":true,"pid":<pid>,"name":"<name>"} or {"found":false}.
#
# microkitchen stages this into every kitchen; the egress broker runs it while
# an approval is pending, when the guest socket is still connecting. The answer
# is shown to a human and never decides anything (design §8). Only the short
# process name is printed, never the command line: arguments carry secrets.
#
# POSIX sh and awk only. PROC_ROOT replaces /proc (tests).

set -u

# The broker gives up after a second; so does the guest.
if [ -z "${MK_WHODIAL_TIMED:-}" ] && command -v timeout >/dev/null 2>&1; then
    MK_WHODIAL_TIMED=1 exec timeout 1 sh "$0" "$@"
fi

proc=${PROC_ROOT:-/proc}

not_found() {
    printf '{"found":false}\n'
    exit 0
}

[ $# -eq 3 ] || not_found
transport=$1
address=$2
port=$3
case $transport in tcp | udp) ;; *) not_found ;; esac
case $port in '' | *[!0-9]*) not_found ;; esac
[ ${#port} -le 5 ] && [ "$port" -le 65535 ] || not_found

# The destination as the kernel prints it, per table family: "4 <hex>:<port>"
# for /proc/net/{tcp,udp} and "6 <hex>:<port>" for the *6 tables, joined with
# ";". Addresses are 32-bit words in host (little-endian) byte order: one for
# IPv4, four for IPv6. An IPv4 destination also matches IPv4-mapped sockets.
targets=$(printf '%s %s\n' "$address" "$port" | awk '
function pad(g) {
    if (length(g) < 1 || length(g) > 4) return "x"
    return substr("0000", 1, 4 - length(g)) g
}
function words(h,    out, w) {
    out = ""
    for (w = 0; w < 4; w++)
        out = out substr(h, w * 8 + 7, 2) substr(h, w * 8 + 5, 2) substr(h, w * 8 + 3, 2) substr(h, w * 8 + 1, 2)
    return out
}
function ipv4(s,    o, i) {
    if (s !~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/) return ""
    split(s, o, ".")
    for (i = 1; i <= 4; i++) if (length(o[i]) > 3 || o[i] + 0 > 255) return ""
    return sprintf("%02X%02X%02X%02X", o[1], o[2], o[3], o[4])
}
function ipv6(s,    at, left, right, L, R, nl, nr, fill, out, i) {
    if (s !~ /:/ || s ~ /[^0-9A-Fa-f:]/) return ""
    at = index(s, "::")
    if (at) {
        left = substr(s, 1, at - 1)
        right = substr(s, at + 2)
        if (index(right, "::")) return ""
        nl = (left == "") ? 0 : split(left, L, ":")
        nr = (right == "") ? 0 : split(right, R, ":")
        fill = 8 - nl - nr
        if (fill < 1) return ""
    } else {
        nl = split(s, L, ":")
        nr = 0
        fill = 0
        if (nl != 8) return ""
    }
    out = ""
    for (i = 1; i <= nl; i++) out = out pad(L[i])
    for (i = 1; i <= fill; i++) out = out "0000"
    for (i = 1; i <= nr; i++) out = out pad(R[i])
    if (out ~ /x/) return ""
    return toupper(out)
}
{
    p = sprintf("%04X", $2)
    if ((h = ipv4($1)) != "") {
        printf "4 %s:%s;", substr(h, 7, 2) substr(h, 5, 2) substr(h, 3, 2) substr(h, 1, 2), p
        printf "6 %s:%s;", words("00000000000000000000FFFF" h), p
    } else if ((h = ipv6($1)) != "") {
        printf "6 %s:%s;", words(h), p
    }
}')
[ -n "$targets" ] || not_found

# One representative process per network namespace, so that sockets inside
# Docker containers are found too.
reps=
seen=' '
for dir in "$proc"/[0-9]*; do
    ns=$(readlink "$dir/ns/net" 2>/dev/null) || continue
    case $seen in *" $ns "*) continue ;; esac
    seen="$seen$ns "
    reps="$reps ${dir##*/}"
done

# "<rep> <inode>" for each socket whose remote end is the destination. TCP:
# any state but LISTEN (SYN_SENT while the broker holds the reply). UDP: only
# connected sockets record a remote end; unconnected ones are not found.
matches=$(for rep in $reps; do
    for family in 4 6; do
        table=$proc/$rep/net/$transport
        [ "$family" = 6 ] && table=${table}6
        [ -r "$table" ] || continue
        awk -v family="$family" -v transport="$transport" -v rep="$rep" -v targets="$targets" '
            BEGIN {
                n = split(targets, t, ";")
                for (i = 1; i <= n; i++)
                    if (substr(t[i], 1, 2) == family " ") want[substr(t[i], 3)] = 1
            }
            NR > 1 && ($3 in want) && $10 ~ /^[1-9][0-9]*$/ && !(transport == "tcp" && $4 == "0A") {
                print rep, $10
            }' "$table"
    done
done)
[ -n "$matches" ] || not_found

# The process holding one of those sockets, searched in the socket's own
# namespace (inode numbers are only unique within one).
result=$(printf '%s\n' "$matches" | while read -r rep inode; do
    ns=$(readlink "$proc/$rep/ns/net" 2>/dev/null) || continue
    for dir in "$proc"/[0-9]*; do
        [ "$(readlink "$dir/ns/net" 2>/dev/null)" = "$ns" ] || continue
        for fd in "$dir"/fd/*; do
            [ "$(readlink "$fd" 2>/dev/null)" = "socket:[$inode]" ] || continue
            name=
            read -r name 2>/dev/null <"$dir/comm"
            name=$(printf '%s' "$name" | tr -cd 'A-Za-z0-9._+-')
            printf '{"found":true,"pid":%s,"name":"%s"}\n' "${dir##*/}" "${name:-?}"
            exit 0
        done
    done
done)

if [ -n "$result" ]; then
    printf '%s\n' "$result"
else
    not_found
fi
