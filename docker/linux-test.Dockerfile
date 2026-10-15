# Test environment for the Linux-only network tests (network namespaces, veth, raw sockets).
# Built and run by `cargo xtask linux-test`; needs a privileged container.
FROM rust:1
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends iproute2 \
 && rm -rf /var/lib/apt/lists/*
