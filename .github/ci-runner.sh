#!/bin/sh
# Cargo runner for the Linux job: test binaries run as root through sudo (network namespaces, raw sockets,
# tcpdump), everything else as it is. The same idea as .devcontainer/test-runner.sh; sudo keeps the user's PATH
# here (a runner's tools are not in its secure_path).
case "$1" in
  */deps/*) exec sudo -E env "PATH=$PATH" "$@" ;;
  *) exec "$@" ;;
esac
