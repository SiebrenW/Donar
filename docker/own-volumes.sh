#!/bin/sh
# Gives `developer` the named volumes for the build output and the cargo registry, which are
# owned by root when an earlier image created them.
for dir in /target /usr/local/cargo/registry; do
  [ -d "$dir" ] || continue
  [ "$(stat -c %U "$dir")" = developer ] || sudo chown -R developer:developer "$dir"
done
