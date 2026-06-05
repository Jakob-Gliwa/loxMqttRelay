#!/bin/sh
set -e

# Prepend the configured log level (if any), then hand off as PID 1 via exec so
# the Python process receives SIGTERM/SIGINT directly for clean shutdown.
if [ -n "${LOG_LEVEL}" ]; then
    set -- --log-level "${LOG_LEVEL}" "$@"
fi

exec loxmqttrelay "$@"
