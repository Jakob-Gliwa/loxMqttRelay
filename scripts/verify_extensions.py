#!/usr/bin/env python3
"""Verify the compiled Rust extensions are packaged for the current architecture.

This guards against silent pure-Python builds (e.g. when setup.py is missing from
the build context): without the native ``_loxmqttrelay`` extension the package
imports fine here but explodes at runtime with ``ModuleNotFoundError``.

Expected variants must mirror setup.py:
  * x86_64/amd64 -> ``optimized`` (x86-64-v3) AND ``compatible`` (generic)
  * everything else -> ``compatible`` only

For each expected variant the matching ``.so``/``.pyd`` must exist *and* load,
exposing every symbol the relay imports from it. Loading happens in isolation
(directly from the file) so the package's ``__init__`` side effects are not
triggered.
"""
from __future__ import annotations

import glob
import importlib.util
import os
import platform
import sys
import sysconfig


def expected_variants() -> list[str]:
    arch = platform.machine().lower()
    if arch in ("x86_64", "amd64"):
        return ["optimized", "compatible"]
    return ["compatible"]


def find_package_dir() -> str:
    candidates = {
        sysconfig.get_paths().get("platlib", ""),
        sysconfig.get_paths().get("purelib", ""),
        *sys.path,
    }
    for base in candidates:
        if not base:
            continue
        pkg = os.path.join(base, "loxmqttrelay")
        if os.path.isdir(pkg):
            return pkg
    raise SystemExit("FAIL: installed 'loxmqttrelay' package not found on sys.path")


def load_extension(so_path: str) -> object:
    spec = importlib.util.spec_from_file_location("_loxmqttrelay", so_path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"FAIL: cannot create import spec for {so_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    pkg_dir = find_package_dir()
    variants = expected_variants()
    print(f"Architecture: {platform.machine()} -> expected variants: {variants}")
    print(f"Package dir: {pkg_dir}")

    failures: list[str] = []
    for variant in variants:
        matches = sorted(
            glob.glob(os.path.join(pkg_dir, variant, "_loxmqttrelay*.so"))
            + glob.glob(os.path.join(pkg_dir, variant, "_loxmqttrelay*.pyd"))
        )
        if not matches:
            failures.append(f"missing native extension for '{variant}' in {pkg_dir}/{variant}")
            continue
        so_path = matches[0]
        try:
            module = load_extension(so_path)
        except BaseException as exc:  # noqa: BLE001 - report any load failure (incl. SIGILL-ish)
            failures.append(f"'{variant}' extension at {so_path} failed to load: {exc!r}")
            continue
        for symbol in ("MiniserverDataProcessor", "MqttClient", "UdpServer", "init_rust_logger"):
            if not hasattr(module, symbol):
                failures.append(f"'{variant}' extension missing symbol '{symbol}'")
        print(f"OK: '{variant}' -> {os.path.basename(so_path)} (loaded, symbols present)")

    if failures:
        print("\nExtension verification FAILED:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1
    print("\nAll expected Rust extensions are present and loadable.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
