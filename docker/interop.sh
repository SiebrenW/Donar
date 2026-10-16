#!/bin/bash
# Runs the `evcc` command against an independent charger implementation in two network
# namespaces joined by a virtual Ethernet pair. Needs root; `cargo xtask interop` starts it.
#
#   interop.sh PARTNER EVCC_BINARY [evcc arguments]
#
# PARTNER: iso15118-python      the open-source `iso15118` library's SECC, plain TCP
#          iso15118-python-tls  the same with TLS 1.3 and mutual authentication, using the
#                               library's own test PKI (secp256r1; created for the run)
#
# Arguments after the program are given to `evcc` after the partner's defaults.
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

partner_args=
start_iso15118_python() { # $@: extra environment for the SECC
  ( cd /tmp && exec ip netns exec "$secc_ns" env NETWORK_INTERFACE=veth0 PROTOCOLS=ISO_15118_20_DC \
      AUTH_MODES=EIM LOG_LEVEL=DEBUG "$@" \
      /opt/iso15118/bin/python -m iso15118.secc.main ) >"$secc_log" 2>&1 &
  secc_pid=$!
  # The partner needs a few seconds to start its EXI codec and its UDP server.
  for _ in $(seq 40); do
    ip netns exec "$secc_ns" ss -lun 2>/dev/null | grep -q ':15118' && break
    sleep 0.5
  done
}

case $partner in
  iso15118-python)
    start_iso15118_python SECC_ENFORCE_TLS=false
    partner_args="--no-tls --connector extended"
    ;;
  iso15118-python-tls)
    # The library reads its PKI from the `iso15118_2` directory whatever the version.
    pki=$(mktemp -d)
    cp -r /opt/iso15118/lib/python3*/site-packages/iso15118/shared/pki/. "$pki"
    ( cd "$pki" && bash create_certs.sh -v iso-2 -p pass123 ) >/dev/null 2>&1 \
      || { echo "creating the test PKI failed" >&2; exit 2; }
    start_iso15118_python SECC_ENFORCE_TLS=true ENABLE_TLS_1_3=true "PKI_PATH=$pki/"
    partner_args="--chain $pki/iso15118_2/certs/oemCertChain.pem --key $pki/iso15118_2/private_keys/oemLeaf.key
      --key-password pass123 --trust $pki/iso15118_2/certs/v2gRootCACert.pem --backwards-compatible --connector extended"
    ;;
  *) echo "unknown partner $partner" >&2; exit 2 ;;
esac

# shellcheck disable=SC2086 # the partner arguments are words
ip netns exec "$evcc_ns" timeout 60 "$evcc" --interface veth0 $partner_args "$@"
status=$?
echo "evcc exit status: $status"
if [ $status -ne 0 ] || [ -n "${INTEROP_SHOW_PARTNER_LOG:-}" ]; then
  echo "----- $partner log -----"; if [ -n "${INTEROP_SHOW_PARTNER_LOG:-}" ]; then cat "$secc_log"; else tail -n 60 "$secc_log"; fi
fi
exit $status
