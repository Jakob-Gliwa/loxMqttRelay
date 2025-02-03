FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
WORKDIR /app
COPY . .

# Install build dependencies, Rust toolchain, and Python dependencies in a single layer
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc \
        python3-dev \
        curl \
        build-essential \
    # Install Rust toolchain
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
    && . "/root/.cargo/env" \
    # Install and build Python dependencies
    && uv venv \
    && . .venv/bin/activate \
    && uv pip install . \
    && uv pip install -e ".[dev]" \
    # Build Rust code with maturin by building a wheel and installing it
    && conda deactivate \
    && PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin develop --uv --release \
    # Build Cython modules if still needed
    && cd src/loxwebsocket/cython_modules \
    && python setup.py build_ext --inplace \
    && cd /app \
    # Clean up Python environment
    && uv pip uninstall $(uv pip freeze | grep -v "^-e" | cut -d= -f1) \
    && uv pip install . \
    # Clean up build dependencies
    && apt-get remove -y gcc python3-dev curl build-essential \
    && apt-get autoremove -y \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* \
    && rm -rf /root/.cargo \
    && rm -rf /root/.rustup

# Set PYTHONPATH to include the src directory
ENV PYTHONPATH=/app/src
ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")
