# Test environment for the Linux-only network tests (network namespaces, veth, raw sockets).
# Used by `cargo xtask linux-test` and by the dev container (.devcontainer/devcontainer.json);
# needs a privileged container. The JDK is for `cargo xtask vectors` (EXIficient reference
# vectors), iproute2 for the namespaces.
FROM rust:1
# The generator formats its output with rustfmt; clippy is for the editor.
RUN rustup component add rustfmt clippy
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends iproute2 default-jdk-headless \
 && rm -rf /var/lib/apt/lists/*
