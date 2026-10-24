#!/usr/bin/env python3
"""Build Map's portable Linux AppImage inside devenv, without compiler caching."""

import argparse
import json
import os
from pathlib import Path
import platform
import shlex
import subprocess

from appimage_bundle import build_appimage, tool_environment

PROJECT_ROOT = Path(__file__).resolve().parent.parent


def load_config(root, env):
    result = subprocess.run(
        ["devenv", "build", "outputs.linux-package"], cwd=root, env=env,
        check=True, capture_output=False, stdout=subprocess.PIPE, text=True,
    )
    outputs = json.loads(result.stdout)
    return json.loads(Path(outputs["outputs.linux-package"]).read_text())


def package_environment(config, environ, root):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("AppImage packaging currently requires x86_64 Linux")
    required = ("target", "cc", "cxx", "ar", "pkgConfigPath", "libraryPath", "gdal", "glibc", "cmakePrefixPath")
    missing = [key for key in required if not isinstance(config.get(key), str) or not config[key]]
    if missing:
        raise ValueError("Missing Linux package configuration: " + ", ".join(missing))
    env = tool_environment(environ)
    for key in list(env):
        if key.startswith(("NIX_CFLAGS_", "NIX_LDFLAGS", "NIX_CC_WRAPPER_FLAGS_SET",
                           "NIX_BINTOOLS_WRAPPER_FLAGS_SET", "PKG_CONFIG_", "TARGET_PKG_CONFIG_", "VCPKG_")):
            env.pop(key)
    for key in ("LIBRARY_PATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "CPPFLAGS",
                "CFLAGS", "CXXFLAGS", "LDFLAGS", "CMAKE_PREFIX_PATH", "CMAKE_INCLUDE_PATH",
                "CMAKE_LIBRARY_PATH", "CMAKE_TOOLCHAIN_FILE"):
        env.pop(key, None)
    target = config["target"]
    target_key = target.upper().replace("-", "_")
    work = Path(root).resolve() / "dist/build/appimage"
    native_flags = shlex.join([
        "-target", f"x86_64-linux-gnu.{config['glibc']}",
        *(f"-isystem{path}" for path in config.get("includePaths", [])),
    ])
    env.update({
        # Empty values also override wrappers configured in Cargo's user config.
        "RUSTC_WRAPPER": "",
        "RUSTC_WORKSPACE_WRAPPER": "",
        "MLN_CMAKE_CXX_LAUNCHER": "",
        "CMAKE_C_COMPILER_LAUNCHER": "",
        "CMAKE_CXX_COMPILER_LAUNCHER": "",
        "CC": config["cc"],
        "CXX": config["cxx"],
        "AR": config["ar"],
        "CXXSTDLIB": "c++",
        # cc-rs otherwise appends an unversioned --target, losing Zig's baseline.
        "CFLAGS": native_flags,
        "CXXFLAGS": native_flags,
        "CMAKE_PREFIX_PATH": config["cmakePrefixPath"],
        f"CARGO_TARGET_{target_key}_LINKER": config["cc"],
        "PKG_CONFIG_PATH": config["pkgConfigPath"],
        "PKG_CONFIG_LIBDIR": config["pkgConfigPath"],
        "PKG_CONFIG_ALL_STATIC": "1",
        "PKG_CONFIG_ALLOW_CROSS": "1",
        "GDAL_HOME": config["gdal"],
        "GDAL_STATIC": "1",
        "MLN_STATIC_DEPS": "1",
        "MLN_PRECOMPILE": "0",
        "MLN_CMAKE_PYTHON_EXECUTABLE": os.path.realpath(os.sys.executable),
        "CARGO_TARGET_DIR": str(work / "target"),
        # nixpkgs' Zig wrapper places its cache under TMPDIR, not XDG_CACHE_HOME.
        "TMPDIR": str(work / "tmp"),
        "CMAKE_GENERATOR": "Ninja",
        "CMAKE_TOOLCHAIN_FILE": str(work / "toolchain.cmake"),
    })
    for key, value in (("CC", config["cc"]), ("CXX", config["cxx"]),
                       ("AR", config["ar"]), ("CXXSTDLIB", "c++")):
        env[f"{key}_{target.replace('-', '_')}"] = value
        env[f"{key}_{target}"] = value
    flags = (environ["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
             if environ.get("CARGO_ENCODED_RUSTFLAGS") else shlex.split(environ.get("RUSTFLAGS", "")))
    flags.extend(f"-Lnative={path}" for path in config["libraryPath"].split(":") if path)
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    env.pop("RUSTFLAGS", None)
    return env


def prepare_build(env):
    work = Path(env["CARGO_TARGET_DIR"]).parent
    Path(env["TMPDIR"]).mkdir(parents=True, exist_ok=True)
    (work / "toolchain.cmake").write_text(
        "set(CMAKE_SYSTEM_NAME Linux)\n"
        "set(CMAKE_SYSTEM_PROCESSOR x86_64)\n"
        "set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)\n"
    )
    if not (Path(env["GDAL_HOME"]) / "lib/libgdal.a").is_file():
        raise ValueError("Portable GDAL static archive is missing")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="realize and check release inputs without building Map")
    args = parser.parse_args()
    try:
        config = load_config(PROJECT_ROOT, tool_environment(os.environ))
        env = package_environment(config, os.environ, PROJECT_ROOT)
        prepare_build(env)
        if args.check:
            for name in ("CC", "CXX", "GDAL_HOME", "CARGO_TARGET_DIR", "MLN_STATIC_DEPS",
                         "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "MLN_CMAKE_CXX_LAUNCHER"):
                print(f"{name}={env[name]}")
            return
        command = ["cargo", "build", "--release", "--locked", "--bin", "map", "--target", config["target"]]
        print(shlex.join(command), flush=True)
        subprocess.run(command, cwd=PROJECT_ROOT, env=env, check=True)
        binary = Path(env["CARGO_TARGET_DIR"]) / config["target"] / "release/map"
        output = build_appimage(binary, config, PROJECT_ROOT, env)
        print(f"Created {output}")
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Linux packaging failed: {error}\n")


if __name__ == "__main__":
    main()
