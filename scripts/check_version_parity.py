#!/usr/bin/env python3
"""Verify the version is stated identically in every file that states it.

Three files carry it and each is authoritative for something different:
``pyproject.toml`` for the wheel, ``src/loxmqttrelay/__init__.py`` for what a
running relay reports, ``Cargo.toml`` for the native extension. The release
workflow derives the published image tags from the git tag, so a drift here
ships an image that calls itself something it is not.

Given a tag argument, the git tag has to agree as well; a leading ``v`` is
stripped. Without one only the files are compared, which is what a local run
before tagging wants.

Nothing is imported: ``import loxmqttrelay`` would load the compiled extension,
which this must not depend on. The assignment is read out of the AST instead.
"""
from __future__ import annotations

import ast
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PYPROJECT = REPO_ROOT / "pyproject.toml"
CARGO = REPO_ROOT / "Cargo.toml"
INIT = REPO_ROOT / "src" / "loxmqttrelay" / "__init__.py"


def toml_version(path: Path, *table: str) -> str:
    with path.open("rb") as fh:
        node = tomllib.load(fh)
    for key in table:
        node = node[key]
    version = node["version"]
    if not isinstance(version, str):
        raise TypeError(f"{path.name}: [{'.'.join(table)}] version is {type(version).__name__}, expected str")
    return version


def dunder_version(path: Path) -> str:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    for node in tree.body:
        if not isinstance(node, ast.Assign):
            continue
        if not any(isinstance(t, ast.Name) and t.id == "__version__" for t in node.targets):
            continue
        if isinstance(node.value, ast.Constant) and isinstance(node.value.value, str):
            return node.value.value
        raise TypeError(f"{path}: __version__ is not a string literal")
    raise LookupError(f"{path}: no __version__ assignment found")


def main(argv: list[str]) -> int:
    if len(argv) > 1:
        print(f"usage: {Path(argv[0]).name} [release-tag]", file=sys.stderr)
        return 2

    sources = {
        "pyproject.toml [project]": toml_version(PYPROJECT, "project"),
        "Cargo.toml [package]": toml_version(CARGO, "package"),
        "src/loxmqttrelay/__init__.py __version__": dunder_version(INIT),
    }
    if argv:
        sources["release tag"] = argv[0].removeprefix("v")

    width = max(len(name) for name in sources)
    for name, version in sources.items():
        print(f"{name:<{width}} = {version}")

    distinct = set(sources.values())
    if len(distinct) > 1:
        print(
            f"\nVersion mismatch: found {sorted(distinct)}. All of the above must be identical.",
            file=sys.stderr,
        )
        return 1
    print(f"\nOK: version {distinct.pop()} stated consistently.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
