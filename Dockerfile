# First ARG declarations (available for FROM)
ARG TARGET=unknown-linux-gnu
ARG BASE_IMAGE=ghcr.io/astral-sh/uv:python3.13-bookworm-slim
ARG OPTIMIZATION_FLAGS
ARG CYTHON_OPT_FLAGS

# -------------------------------------
# 1) Build-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim as builder
# Redeclare the ARGs needed in this stage
ARG TARGET
ARG OPTIMIZATION_FLAGS
ENV RUSTFLAGS="$OPTIMIZATION_FLAGS"

# System-Tools für Build installieren
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc python3-dev curl build-essential python3-pandas \
    && if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then \
            apt-get install -y gcc-aarch64-linux-gnu libc6-dev-arm64-cross clang && \
            ln -sf /usr/bin/aarch64-linux-gnu-clang /usr/bin/cc; \
        fi


# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
&& echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc

ENV PATH="/root/.cargo/bin:${PATH}"
ENV PYO3_PRINT_CONFIG=1
RUN export RUSTFLAGS="$OPTIMIZATION_FLAGS"

# Fügen Sie das gewünschte Rust-Ziel hinzu
RUN if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then \
        rustup target add aarch64-unknown-linux-gnu; \
    fi

    # Konfiguriere Cargo, um den ARM64 Linker zu verwenden
RUN if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then \
mkdir -p ~/.cargo && \
echo "[target.aarch64-unknown-linux-gnu]" > ~/.cargo/config.toml && \
echo "linker = \"aarch64-linux-gnu-clang\"" >> ~/.cargo/config.toml && \
echo "ar = \"aarch64-linux-gnu-ar\"" >> ~/.cargo/config.toml; \
fi

WORKDIR /app
COPY . .

# Create and use virtual environment with uv
RUN uv venv && uv pip install -v ".[build]" --only-binary=pandas

# Build Cython modules with the passed CYTHON_OPT_FLAGS
RUN cd src/loxwebsocket/cython_modules && \
    CYTHON_OPT_FLAGS="$CYTHON_OPT_FLAGS" uv run python setup.py build_ext --inplace && \
    cd ../../..

# Build wheel (Python + Rust)
RUN if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then \
         export RUSTFLAGS="$OPTIMIZATION_FLAGS"; \
         cargo clean; \
         PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 uv run maturin develop --uv --release --target aarch64-unknown-linux-gnu; \
     else \
         export RUSTFLAGS="$OPTIMIZATION_FLAGS"; \
         cargo clean; \
         PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 uv run maturin develop -vv --uv --target x86_64-unknown-linux-gnu -C target-cpu=generic; \
     fi

# -------------------------------------
# 2) Final-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
WORKDIR /app

# Only copy wheels and project files from the builder stage
COPY --from=builder /app/pyproject.toml /app/pyproject.toml
COPY --from=builder /app/Cargo.toml /app/Cargo.toml
COPY --from=builder /app/src /app/src
COPY --from=builder /app/.venv /app/.venv

# ENV PYTHONPATH=/app/src

ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD . .venv/bin/activate && exec .venv/bin/loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")