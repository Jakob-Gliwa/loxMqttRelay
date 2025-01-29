FROM ghcr.io/astral-sh/uv:python3.13-alpine
WORKDIR /app
COPY . .
# Install build dependencies, Python dependencies, and clean up in a single layer
RUN apk add --no-cache --virtual .build-deps \
    gcc \
    musl-dev \
    python3-dev \
 && uv pip install --system . \
 && uv pip install --system -e ".[dev]" \
 && cd src/loxwebsocket/cython_modules \
 && python setup.py build_ext --inplace \
 && cd /app \
 && uv pip freeze | grep -v "^-e" | cut -d= -f1 | xargs -r uv pip uninstall --system -y \
 && apk add --no-cache \
    py3-pyarrow \
 && uv pip install --system . \
 && apk del .build-deps \
 && rm -rf /var/cache/apk/*

# Set PYTHONPATH to include the src directory
ENV PYTHONPATH=/app/src
ENV HEADLESS=false
ENV LOG_LEVEL=INFO
EXPOSE 11884/udp
EXPOSE 8501/tcp
CMD loxmqttrelay $([ "$HEADLESS" = "true" ] && echo "--headless") $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL")
