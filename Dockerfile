# syntax=docker/dockerfile:1
# -------------------------------------
# 1) Build-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.14-bookworm-slim AS builder

# - UV_PYTHON_DOWNLOADS=0: use the image's system Python at
#   /usr/local/bin/python3.14. That path is identical in the final
#   python:3.14-slim image, so the copied venv stays valid.
# - UV_COMPILE_BYTECODE=1: precompile .pyc for faster cold starts.
# - UV_LINK_MODE=copy: materialize real files in the venv (do NOT hardlink into
#   the cache mount, which is not part of the image layer).
ENV UV_PYTHON_DOWNLOADS=0 \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    VIRTUAL_ENV=/app/.venv \
    CARGO_TARGET_DIR=/build/cargo-target \
    PATH="/root/.cargo/bin:${PATH}"

# Build toolchain: build-essential (gcc/g++ for the Rust ext + pygixml C++),
# python headers and curl (for rustup).
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3-dev curl build-essential \
    && rm -rf /var/lib/apt/lists/*

# Modern stable Rust toolchain via rustup (understands Cargo.lock v4).
# Downloaded to a file rather than piped into sh: /bin/sh here is dash, which
# has no pipefail, so a curl that fails feeds sh an empty script and the layer
# succeeds without a compiler. The failure would then surface far downstream as
# setuptools-rust's "can't find Rust compiler".
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --default-toolchain stable --profile minimal \
    && rm /tmp/rustup-init.sh \
    && cargo --version

WORKDIR /app
RUN uv venv

# --- Layer A: third-party dependencies only (cache-friendly) ---
# Re-runs only when pyproject.toml changes, so the expensive dependency install
# (incl. the pygixml source build) is reused across source edits.
#
# pygixml has no arm64 wheel and its x86_64 wheel is compiled with AVX2 (no
# runtime CPU dispatch); the arm64 source build otherwise bakes in -march=native
# tuned to the build host. Both SIGILL (exit 132) on weaker CPUs — non-AVX2 x86
# and Raspberry Pi. Build pygixml PORTABLY so one binary runs on every CPU of the
# image's architecture:
#   * --no-binary pygixml  -> force a source build (skip the prebuilt AVX2 wheel)
#   * CI=1                  -> pygixml's Optimize.cmake then omits -march=native
COPY pyproject.toml ./
RUN --mount=type=cache,target=/root/.cache/uv \
    CI=1 uv pip install -r pyproject.toml --no-binary pygixml

# --- Layer B: build & install our own project (Rust extension) ---
# Non-editable, so the package (incl. the compiled extension) lands directly in
# the venv's site-packages — the final image needs no source tree.
# setup.py is REQUIRED: the Rust extensions are declared there (not in
# pyproject.toml). Without it the build silently produces a pure-Python wheel
# with no .so, and importing loxmqttrelay.{optimized,compatible} fails at runtime.
# build.rs is equally required - cargo refuses to build without the file the
# manifest implies.
COPY setup.py Cargo.toml build.rs ./
COPY src ./src
RUN --mount=type=cache,target=/root/.cache/uv \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/cargo-target \
    uv pip install --no-deps .

# Strip debug symbols from native extensions to shrink the venv.
RUN find /app/.venv -name '*.so' -exec strip --strip-unneeded {} + || true

# Fail the build early if the expected native extensions for this architecture
# are missing or unloadable (e.g. an accidental pure-Python wheel). Uses the
# venv interpreter so it inspects what actually got installed.
COPY scripts/verify_extensions.py ./scripts/verify_extensions.py
RUN /app/.venv/bin/python scripts/verify_extensions.py

# -------------------------------------
# 2) Final-Stage (no uv, no build tools)
# -------------------------------------
FROM python:3.14-slim-bookworm
WORKDIR /app

ENV LOG_LEVEL=INFO \
    PATH="/app/.venv/bin:${PATH}"

# The project is installed non-editably into the venv's site-packages, so the
# runtime image only needs the venv — no source tree, pyproject or Cargo.toml.
COPY --from=builder /app/.venv /app/.venv

COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

EXPOSE 11884/udp

# Docker's default, stated so the contract is visible: the relay installs a
# SIGTERM handler and shuts the MQTT session down before exiting.
STOPSIGNAL SIGTERM

ENTRYPOINT ["docker-entrypoint.sh"]
