# Test environment for the Linux-only network tests (network namespaces, veth, raw sockets).
# Used by `cargo xtask linux-test` and by the dev container (.devcontainer/devcontainer.json);
# the container has to be privileged. The JDK is for `cargo xtask vectors` (EXIficient
# reference vectors), iproute2 and sudo for the namespaces, Python for the interop partner, Chromium for the
# tests of the web page.
FROM rust:1
# The generator formats its output with rustfmt; clippy is for the editor.
RUN rustup component add rustfmt clippy
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends iproute2 default-jdk-headless sudo python3-venv chromium \
 && rm -rf /var/lib/apt/lists/*

# An independent ISO 15118-20 charger (SECC) to test against, `cargo xtask interop`: the
# open-source `iso15118` Python library, which brings its own EXI codec (needs the JDK).
RUN python3 -m venv /opt/iso15118 && /opt/iso15118/bin/pip install --quiet iso15118==0.31.2

# An independent SLAC implementation for the EVSE side, `cargo xtask interop`: pyslac (ISO 15118-3), pinned.
RUN python3 -m venv /opt/pyslac \
 && /opt/pyslac/bin/pip install --quiet "marshmallow<4" "git+https://github.com/ecog-io/pyslac@134cb15dfca6381892ef8e30d15e615dc9c61c50"

# Everything runs as `developer`, who may use sudo without a password.
RUN useradd --create-home --uid 1000 --shell /bin/bash developer \
 && echo 'developer ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/developer \
 && chmod 0440 /etc/sudoers.d/developer

# Cargo's home is writable for any user, since editors may change `developer`'s UID to the
# host user's. The named volumes for the build output and the cargo registry start out empty
# and take the ownership of these directories; `own-volumes` repairs the rest.
RUN mkdir -p /target \
 && chown developer:developer /target \
 && chmod -R a+rwX /usr/local/cargo
COPY own-volumes.sh /usr/local/bin/own-volumes
COPY test-runner.sh /usr/local/bin/test-runner
RUN chmod 0755 /usr/local/bin/own-volumes /usr/local/bin/test-runner

# Test binaries run as root through sudo (namespaces, raw sockets); everything else, such as
# `cargo xtask`, runs as `developer` and does not leave root-owned files in the repository.
ENV EVCC_REQUIRE_BROWSER=1 \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER=/usr/local/bin/test-runner \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER=/usr/local/bin/test-runner \
    CARGO_TARGET_DIR=/target

USER developer
