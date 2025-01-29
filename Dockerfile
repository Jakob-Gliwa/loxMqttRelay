FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
WORKDIR /app
COPY . .
# Install build dependencies, Python dependencies, and clean up in a single layer
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc \
        python3-dev \
    && uv pip install --system . \
    && uv pip install --system -e ".[dev]" \
    && cd src/loxwebsocket/cython_modules \
    && python setup.py build_ext --inplace \
    && cd /app \
    && uv pip uninstall --system $(uv pip freeze | grep -v "^-e" | cut -d= -f1) \
    && uv pip install --system . \
    && apt-get remove -y gcc python3-dev \
    && apt-get autoremove -y \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*
# Set PYTHONPATH to include the src directory
ENV PYTHONPATH=/app/src
ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")
