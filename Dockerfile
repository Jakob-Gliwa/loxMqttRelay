# -------------------------------------
# 1) Build-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim as builder

# System-Tools für Build installieren
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc python3-dev curl build-essential 

# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
&& echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc

ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /app
COPY . .

# Create and use virtual environment with uv
RUN uv pip install . --system && \
    uv pip install -e ".[dev]" --system

# Wheel bauen (Python + Rust)
RUN PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin build --release --compatibility off && uv pip install target/wheels/loxmqttrelay-*.whl --system

# Build Cython modules if still needed
RUN cd src/loxwebsocket/cython_modules \
    && python setup.py build_ext --inplace 

# -------------------------------------
# 2) Final-Stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
WORKDIR /app

# Nur Wheels aus der Build-Stage kopieren
COPY --from=builder /app/target/wheels/*.whl /tmp/
COPY --from=builder /app/src /app/src
COPY --from=builder /app/pyproject.toml /app/pyproject.toml

RUN uv pip install . --system && uv pip install /tmp/loxmqttrelay-*.whl --system && rm /tmp/*.whl

# Set PYTHONPATH to include the src directory
ENV PYTHONPATH=/app/src
ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")