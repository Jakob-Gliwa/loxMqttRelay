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
RUN uv venv && uv pip install -v . --only-binary=pandas

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