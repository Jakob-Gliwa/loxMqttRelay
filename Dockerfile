ARG TARGET=unknown-linux-gnu
ARG BASE_IMAGE=ghcr.io/astral-sh/uv:python3.13-bookworm-slim

# -------------------------------------
# 1) Build-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim as builder

# System-Tools für Build installieren
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc python3-dev curl build-essential  \
    && if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then \
            apt-get install -y gcc-aarch64-linux-gnu libc6-dev-arm64-cross clang && \
            ln -sf /usr/bin/aarch64-linux-gnu-clang /usr/bin/cc; \
        fi


# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
&& echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc

ENV PATH="/root/.cargo/bin:${PATH}"

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
RUN uv pip install ".[build]" --system

    # Build Cython modules if still needed
RUN cd src/loxwebsocket/cython_modules \
    && python setup.py build_ext --inplace 

# Wheel bauen (Python + Rust)
RUN if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then \
         PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin build --release --compatibility off --target aarch64-unknown-linux-gnu; \
     else \
         PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin build --release --compatibility off; \
     fi

# -------------------------------------
# 2) Final-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
WORKDIR /app

# Only copy wheels and project files from the builder stage
COPY --from=builder /app/target/wheels/*.whl /tmp/
COPY --from=builder /app/pyproject.toml /app/pyproject.toml
COPY --from=builder /app/Cargo.toml /app/Cargo.toml
COPY --from=builder /app/src /app/src

# Install the built wheel (now with a proper ARM64 or x86_64 Python)
RUN pip install --no-cache-dir /tmp/loxmqttrelay-*.whl && \
    rm /tmp/*.whl

# ENV PYTHONPATH=/app/src

ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")