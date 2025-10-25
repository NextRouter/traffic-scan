#!/usr/bin/env bash
# Configure nftables + policy routing so that traffic from LAN (eth2)
# is split by client IP parity: even -> eth0, odd -> eth1.
# - eth0, eth1 receive IPs via DHCP
# - eth2 serves clients (e.g., 10.40.0.0/20 via ISC DHCP)
#
# Safe to re-run; it flushes only what it manages (its tables/rules).
#
# Usage:
#   sudo ./scripts/setup_nftables_parity_routing.sh
#
# Optional env overrides:
#   WAN_EVEN_IF=eth0 WAN_ODD_IF=eth1 LAN_IF=eth2 ./scripts/setup_nftables_parity_routing.sh
#
# Notes:
# - If eth0/eth1 DHCP renew changes gateway, re-run this script.
# - To persist across reboots, add this to a systemd service or dhclient hook.

set -euo pipefail

WAN_EVEN_IF=${WAN_EVEN_IF:-eth0}   # even client IPs go out here
WAN_ODD_IF=${WAN_ODD_IF:-eth1}     # odd client IPs go out here
LAN_IF=${LAN_IF:-eth2}

TABLE_EVEN=100
TABLE_ODD=101
MARK_EVEN=0x2
MARK_ODD=0x1

NFT_TBL_MANGLE=parity_mangle
NFT_TBL_FILTER=parity_filter
NFT_TBL_NAT=parity_nat

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || { echo "Error: missing required command: $1" >&2; exit 1; }
}

require_root() {
  if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    echo "This script must be run as root." >&2
    exit 1
  fi
}

get_default_gw_for_if() {
  # Prints the IPv4 default gw for the given interface, if present
  local ifc=$1
  # Format: default via 192.168.1.1 dev eth0 ...
  ip -4 route show default | awk -v IF="$ifc" '$0 ~ (" dev " IF "($| )") {for (i=1;i<=NF;i++) if ($i=="via") {print $(i+1); exit}}'
}

get_lan_cidr() {
  # Returns first IPv4 address/prefix on LAN_IF (e.g., 10.40.0.1/20)
  ip -o -f inet addr show dev "$LAN_IF" | awk '{print $4}' | head -n1
}

setup_sysctl() {
  echo "Enabling IPv4 forwarding" >&2
  sysctl -w net.ipv4.ip_forward=1 >/dev/null
}

flush_old_routes_rules() {
  # Remove our specific fwmark rules and flush our tables
  ip -4 rule del fwmark $MARK_EVEN table $TABLE_EVEN 2>/dev/null || true
  ip -4 rule del fwmark $MARK_ODD table $TABLE_ODD 2>/dev/null || true

  ip -4 route flush table $TABLE_EVEN 2>/dev/null || true
  ip -4 route flush table $TABLE_ODD 2>/dev/null || true
}

setup_routes_rules() {
  local gw_even gw_odd lan_cidr
  gw_even=$(get_default_gw_for_if "$WAN_EVEN_IF" || true)
  gw_odd=$(get_default_gw_for_if "$WAN_ODD_IF" || true)
  lan_cidr=$(get_lan_cidr || true)

  if [[ -z "$gw_even" && -z "$gw_odd" ]]; then
    echo "Warning: No default gateways found on $WAN_EVEN_IF or $WAN_ODD_IF. Routing may fail." >&2
  fi

  if [[ -n "$lan_cidr" ]]; then
    # Ensure LAN reachability from our custom tables
    ip -4 route add "$lan_cidr" dev "$LAN_IF" table $TABLE_EVEN 2>/dev/null || true
    ip -4 route add "$lan_cidr" dev "$LAN_IF" table $TABLE_ODD 2>/dev/null || true
  else
    echo "Warning: Could not determine LAN prefix on $LAN_IF; skipping connected route in custom tables." >&2
  fi

  if [[ -n "$gw_even" ]]; then
    ip -4 route add default via "$gw_even" dev "$WAN_EVEN_IF" table $TABLE_EVEN 2>/dev/null || \
    ip -4 route replace default via "$gw_even" dev "$WAN_EVEN_IF" table $TABLE_EVEN
  fi

  if [[ -n "$gw_odd" ]]; then
    ip -4 route add default via "$gw_odd" dev "$WAN_ODD_IF" table $TABLE_ODD 2>/dev/null || \
    ip -4 route replace default via "$gw_odd" dev "$WAN_ODD_IF" table $TABLE_ODD
  fi

  # Add policy rules based on fwmarks
  ip -4 rule add fwmark $MARK_EVEN table $TABLE_EVEN priority 10000 2>/dev/null || true
  ip -4 rule add fwmark $MARK_ODD table $TABLE_ODD priority 10001 2>/dev/null || true
}

apply_nftables() {
  # Remove our previous tables (ignore errors)
  nft list table inet $NFT_TBL_MANGLE >/dev/null 2>&1 && nft delete table inet $NFT_TBL_MANGLE || true
  nft list table inet $NFT_TBL_FILTER >/dev/null 2>&1 && nft delete table inet $NFT_TBL_FILTER || true
  nft list table ip   $NFT_TBL_NAT    >/dev/null 2>&1 && nft delete table ip   $NFT_TBL_NAT    || true

  # Apply new ruleset
  nft -f - <<EOF
# Mangle: mark packets by source IP parity as they arrive on LAN_IF
# Using bitwise AND on IPv4 address: LSB 1=odd, 0=even
# Note: nft uses network byte order; the least significant bit still corresponds to the last octet LSB.
table inet $NFT_TBL_MANGLE {
  chain preroute_mark {
    type filter hook prerouting priority -150; policy accept;
    iifname "$LAN_IF" ip saddr & 0x00000001 == 0x00000000 meta mark set $MARK_EVEN
    iifname "$LAN_IF" ip saddr & 0x00000001 == 0x00000001 meta mark set $MARK_ODD
  }
}

# Filter: basic forwarding policy
table inet $NFT_TBL_FILTER {
  chain forward {
    type filter hook forward priority 0; policy drop;

    # Allow established/related both ways
    ct state established,related accept

    # Allow LAN -> WAN
    iifname "$LAN_IF" oifname "$WAN_EVEN_IF" accept
    iifname "$LAN_IF" oifname "$WAN_ODD_IF"  accept

    # Allow replies WAN -> LAN
    iifname "$WAN_EVEN_IF" oifname "$LAN_IF" ct state established,related accept
    iifname "$WAN_ODD_IF"  oifname "$LAN_IF" ct state established,related accept
  }
}

# NAT: masquerade per egress WAN interface
 table ip $NFT_TBL_NAT {
  chain postrouting {
    type nat hook postrouting priority 100; policy accept;
    oifname "$WAN_EVEN_IF" masquerade
    oifname "$WAN_ODD_IF"  masquerade
  }
}
EOF
}

print_summary() {
  echo "=== Summary ==="
  echo "LAN_IF:        $LAN_IF"
  echo "WAN_EVEN_IF:   $WAN_EVEN_IF (fwmark $MARK_EVEN -> table $TABLE_EVEN)"
  echo "WAN_ODD_IF:    $WAN_ODD_IF  (fwmark $MARK_ODD  -> table $TABLE_ODD)"
  echo
  echo "ip rule:"; ip -4 rule show | sed 's/^/  /'
  echo
  echo "Table $TABLE_EVEN:"; ip -4 route show table $TABLE_EVEN | sed 's/^/  /'
  echo "Table $TABLE_ODD:";  ip -4 route show table $TABLE_ODD  | sed 's/^/  /'
  echo
  echo "nftables tables:"; nft list tables | sed 's/^/  /'
}

main() {
  need_cmd ip
  need_cmd nft
  need_cmd sysctl
  require_root

  setup_sysctl
  flush_old_routes_rules
  setup_routes_rules
  apply_nftables
  print_summary
}

main "$@"
