FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
WORKDIR /app
COPY . .

# Install build dependencies, Rust toolchain, and Python dependencies in a single layer
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc \
        python3-dev \
        curl \
        build-essential
    # Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
    && echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc

ENV PATH="/root/.cargo/bin:${PATH}"
    
# Create and use virtual environment with uv
RUN uv venv && uv pip install . --venv && uv pip install -e ".[dev]" --venv

# Build Rust code with maturin
RUN PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 uv run maturin develop --uv --release
    # Build Cython modules if still needed
RUN cd src/loxwebsocket/cython_modules \
    && uv run python setup.py build_ext --inplace 
    # Clean up build dependencies
RUN apt-get remove -y gcc python3-dev curl build-essential && apt-get autoremove -y && apt-get clean && rm -rf /var/lib/apt/lists/* /root/.cargo /root/.rustup

# Set PYTHONPATH to include the src directory
ENV PYTHONPATH=/app/src
ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD uv run loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")
