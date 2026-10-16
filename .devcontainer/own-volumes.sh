#!/bin/sh
# Gives the current user (`developer`, or whatever UID an editor changed that to) the named
# volumes for the build output and the cargo registry, which belong to another user when an
# earlier image or UID created them.
for dir in /target /usr/local/cargo/registry; do
  [ -d "$dir" ] || continue
  [ "$(stat -c %u "$dir")" = "$(id -u)" ] || sudo chown -R "$(id -u):$(id -g)" "$dir"
done
