#!/usr/bin/env python3
"""Prepare the native source revision required by the locked MapLibre crate."""

import json
import subprocess
import sys
import tomllib
from pathlib import Path


def prepare(destination):
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1",
    ]))
    package = next(p for p in metadata["packages"] if p["name"] == "maplibre_native")
    with Path(package["manifest_path"]).open("rb") as manifest:
        revision = tomllib.load(manifest)["package"]["metadata"]["mln"]["release"]
    if not destination.exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([
            "git", "clone", "--depth", "1", "--branch", revision,
            "https://github.com/maplibre/maplibre-native.git", str(destination),
        ], check=True)
    head = subprocess.check_output(["git", "-C", str(destination), "rev-parse", "HEAD"], text=True).strip()
    expected = subprocess.run([
        "git", "-C", str(destination), "rev-parse", "--verify", f"{revision}^{{commit}}",
    ], check=False, capture_output=True, text=True)
    if expected.returncode != 0 or head != expected.stdout.strip():
        raise RuntimeError(f"Native source at {destination} must be at {revision}")
    subprocess.run([
        "git", "-C", str(destination), "submodule", "update", "--init", "--recursive",
        "--depth", "1", "--jobs", "8",
    ], check=True)


if __name__ == "__main__":
    try:
        prepare(Path(sys.argv[1]))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        sys.exit(f"Could not prepare MapLibre Native: {error}")
