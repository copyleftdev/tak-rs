#!/usr/bin/env bash
# Capture a TAK Server TLS handshake from a real ATAK / iTAK / WinTAK
# client for confirmation analysis (per docs/decisions/0002-tls-ciphers.md).
#
# Usage:
#   sudo scripts/capture-atak-handshake.sh [port=8089] [output=/tmp/atak-<ts>.pcap]
#
# Prereqs:
#   - tcpdump (apt: tcpdump)
#   - tshark optional, for analysis (apt: tshark)
#   - root or CAP_NET_RAW for tcpdump
#   - Either a TAK server running on $port, or the script run on the same
#     host and the ATAK client pointed at this machine's address.
#
# After capture, analyze with:
#   tshark -r <pcap> -Y 'tls.handshake.type==1' -V \
#     | grep -E 'Cipher Suite:|Version:|Extension'
#
# A successful handshake means every cipher the client offers will match
# at least one of the three approved suites in ADR 0002. If the
# ClientHello shows ZERO matches, that's a regression worth documenting.

set -euo pipefail

port="${1:-8089}"
default_out="/tmp/atak-handshake-$(date +%Y%m%d-%H%M%S).pcap"
out="${2:-$default_out}"

if [ "$(id -u)" -ne 0 ]; then
    echo "needs root for raw socket capture; re-run as: sudo $0 $*" >&2
    exit 1
fi

if ! command -v tcpdump >/dev/null 2>&1; then
    echo "tcpdump not installed; install with: apt install tcpdump" >&2
    exit 1
fi

iface="$(ip route 2>/dev/null | awk '/default/ {print $5; exit}')"
if [ -z "$iface" ]; then
    echo "no default route interface found; specify manually with TAK_IFACE=ethX" >&2
    iface="${TAK_IFACE:-any}"
fi

echo "Capture target:  TCP port $port on interface $iface"
echo "Output:          $out"
echo "Stop:            Ctrl-C (or send SIGINT to tcpdump)"
echo

# Capture both directions of port $port. -s 0 = full-frame capture (no truncation).
tcpdump -i "$iface" -s 0 -w "$out" "tcp port $port"

echo
echo "Wrote: $out"
echo
if command -v tshark >/dev/null 2>&1; then
    echo "Quick analysis (ClientHello cipher offerings):"
    tshark -r "$out" -Y 'tls.handshake.type==1' -V 2>/dev/null \
        | grep -E 'Cipher Suite:|Version:' | head -40
else
    echo "Install tshark for analysis: apt install tshark"
fi
