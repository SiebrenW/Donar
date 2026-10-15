#!/bin/bash
# Runs the `evcc` command against an independent charger implementation in two network
# namespaces joined by a virtual Ethernet pair. Needs root; `cargo xtask interop` starts it.
#
#   interop.sh PARTNER EVCC_BINARY [evcc arguments]
#
# PARTNER: iso15118-python (the open-source `iso15118` library's SECC, plain TCP).
set -u
partner=$1; evcc=$2; shift 2
id=$$
secc_ns=secc$id; evcc_ns=evcc$id
secc_log=$(mktemp); secc_pid=

cleanup() {
  [ -n "$secc_pid" ] && kill "$secc_pid" 2>/dev/null
  pkill -f "netns/$secc_ns" 2>/dev/null
  ip netns del "$secc_ns" 2>/dev/null; ip netns del "$evcc_ns" 2>/dev/null
}
trap cleanup EXIT

ip netns add "$secc_ns"; ip netns add "$evcc_ns"
ip link add "va$id" type veth peer name "vb$id"
ip link set "va$id" netns "$evcc_ns"; ip link set "vb$id" netns "$secc_ns"
ip -n "$evcc_ns" link set "va$id" name veth0; ip -n "$secc_ns" link set "vb$id" name veth0
ip -n "$evcc_ns" addr add fe80::1/64 dev veth0 nodad; ip -n "$secc_ns" addr add fe80::2/64 dev veth0 nodad
# No duplicate address detection: chargers that bind to their link-local address during SDP
# would otherwise answer a request only after the half second the check takes.
for ns in "$evcc_ns" "$secc_ns"; do
  ip netns exec "$ns" sysctl -q -w net.ipv6.conf.veth0.accept_dad=0
  ip -n "$ns" link set veth0 up; ip -n "$ns" link set lo up
done

case $partner in
  iso15118-python)
    ( cd /tmp && exec ip netns exec "$secc_ns" env NETWORK_INTERFACE=veth0 PROTOCOLS=ISO_15118_20_DC \
        AUTH_MODES=EIM SECC_ENFORCE_TLS=false LOG_LEVEL=DEBUG \
        /opt/iso15118/bin/python -m iso15118.secc.main ) >"$secc_log" 2>&1 &
    secc_pid=$!
    # The partner needs a few seconds to start its EXI codec and its UDP server.
    for _ in $(seq 40); do
      ip netns exec "$secc_ns" ss -lun 2>/dev/null | grep -q ':15118' && break
      sleep 0.5
    done
    ;;
  *) echo "unknown partner $partner" >&2; exit 2 ;;
esac

ip netns exec "$evcc_ns" timeout 60 "$evcc" --interface veth0 "$@"
status=$?
echo "evcc exit status: $status"
if [ $status -ne 0 ] || [ -n "${INTEROP_SHOW_PARTNER_LOG:-}" ]; then
  echo "----- $partner log -----"; if [ -n "${INTEROP_SHOW_PARTNER_LOG:-}" ]; then cat "$secc_log"; else tail -n 60 "$secc_log"; fi
fi
exit $status
