# syntax=docker/dockerfile:1
# -------------------------------------
# 1) Build stage
# -------------------------------------
FROM rust:1-slim-bookworm AS builder

# musl, so the result is a static binary with no loader and no libc to ship. That
# is what makes the final stage `scratch` possible; against glibc it would need a
# base image whose libc matches the builder's.
#
# musl-tools provides musl-gcc, which `ring` needs for its handful of C files.
RUN apt-get update && apt-get install -y --no-install-recommends \
        musl-tools ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG TARGETARCH
WORKDIR /build

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src

# Two relay builds on x86_64, one on everything else, plus the launcher that
# picks between them.
#
# Both relay builds are compiled at opt-level 3, so a comparison between them
# measures the instruction set and nothing else.
#
# `-C target-feature=+crt-static` is the default for musl targets but is stated
# here so a future target change cannot silently produce a dynamic binary that
# `scratch` then cannot run.
#
# On a non-amd64 build the generic binary is copied to the optimized name as
# well, so the launcher can look for both names unconditionally.
#
# The `ldd` loop at the end is not paranoia: a dynamically linked binary builds
# and runs fine here and then fails in `scratch` with nothing but "no such file
# or directory", which points at the path rather than at the linkage.
#
# NOTE: no `#` comments inside the RUN below. Docker joins the continued lines
# into one before handing them to the shell, and a `#` there comments out
# everything that follows it - including the rest of the script.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    set -eux; \
    case "${TARGETARCH}" in \
      amd64) TARGET=x86_64-unknown-linux-musl ;; \
      arm64) TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "${TARGET}"; \
    mkdir -p /out; \
    \
    RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=generic" \
      cargo build --release --locked --target "${TARGET}" \
        --bin loxmqttrelay-relay --bin loxmqttrelay; \
    cp "target/${TARGET}/release/loxmqttrelay-relay" /out/loxmqttrelay-relay-generic; \
    cp "target/${TARGET}/release/loxmqttrelay" /out/loxmqttrelay; \
    \
    if [ "${TARGETARCH}" = "amd64" ]; then \
      RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=x86-64-v3" \
        cargo build --release --locked --target "${TARGET}" --bin loxmqttrelay-relay; \
      cp "target/${TARGET}/release/loxmqttrelay-relay" /out/loxmqttrelay-relay-v3; \
    else \
      cp /out/loxmqttrelay-relay-generic /out/loxmqttrelay-relay-v3; \
    fi; \
    for binary in /out/*; do \
      if ldd "${binary}" 2>&1 | grep -q "=>"; then \
        echo "${binary} is dynamically linked and will not run in scratch" >&2; \
        exit 1; \
      fi; \
    done; \
    /out/loxmqttrelay-relay-generic --version

# -------------------------------------
# 2) Final stage
# -------------------------------------
FROM scratch

# LOAD-BEARING. `--config` defaults to the *relative* `config/config.toml`, so
# the working directory decides where that lands: /app here, which is where the
# documented `-v ./config:/app/config` mount puts it. Without this the relay
# looks in /config, finds nothing, and starts on the defaults - a container that
# comes up, ignores the operator's configuration and dials 127.0.0.1. The smoke
# test in ci.yml exists because that failure is invisible from the outside.
WORKDIR /app

# The only thing the binaries need from a filesystem. Both TLS stacks verify
# against these when the Miniserver is on 443 or the broker speaks TLS; without
# them every such connection fails with a certificate error.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

COPY --from=builder /out/ /usr/local/bin/

# Read by the relay directly; there is no entrypoint script to translate it.
ENV LOG_LEVEL=INFO

EXPOSE 11884/udp

# Docker's default, stated so the contract is visible: the relay installs a
# SIGTERM handler and shuts the MQTT session down before exiting.
STOPSIGNAL SIGTERM

# The launcher, which picks the build this CPU can run and execs it. Set
# LOXMQTTRELAY_BUILD=generic to override.
ENTRYPOINT ["/usr/local/bin/loxmqttrelay"]
