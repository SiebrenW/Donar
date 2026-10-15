#!/bin/sh
# Cargo runner for the Linux test container: test binaries (they live in a `deps` directory)
# run as root, other programs run as they are.
case "$1" in
  */deps/*) exec sudo -E "$@" ;;
  *) exec "$@" ;;
esac
