#!/usr/bin/env python3
"""Cross-compile Map's Windows distribution on Linux using pkgs-mod MinGW."""

import argparse
import json
import os
import platform
import re
import shlex
import shutil
import subprocess
from pathlib import Path

from appimage_bundle import tool_environment

PROJECT_ROOT = Path(__file__).resolve().parent.parent
TARGET = "x86_64-pc-windows-gnu"
TARGET_KEY = TARGET.replace("-", "_")


def load_config(root, env):
    result = subprocess.run(
        ["devenv", "build", "outputs.windows-package"], cwd=root, env=env,
        check=True, stdout=subprocess.PIPE, text=True,
    )
    outputs = json.loads(result.stdout)
    return json.loads(Path(outputs["outputs.windows-package"]).read_text())


def package_environment(config, environ, root):
    if platform.system() != "Linux":
        raise ValueError("Windows cross-compilation requires Linux")
    required = ("target", "cc", "cxx", "ar", "rc", "objdump", "magick", "toolPath",
                "pkgConfigPath", "libraryPath", "cmakePrefixPath", "bindgenArgs", "cflags",
                "gdal", "gpuiWindows", "gpuiPreWindows")
    missing = [key for key in required if not isinstance(config.get(key), str) or not config[key]]
    if missing:
        raise ValueError("Missing Windows package configuration: " + ", ".join(missing))
    if config["target"] != TARGET:
        raise ValueError(f"Unsupported Windows target: {config['target']}")
    env = tool_environment(environ)
    for key in list(env):
        if key.startswith(("NIX_CFLAGS_", "NIX_LDFLAGS", "NIX_CC_WRAPPER_FLAGS_SET",
                           "NIX_BINTOOLS_WRAPPER_FLAGS_SET", "PKG_CONFIG_",
                           "TARGET_PKG_CONFIG_", "VCPKG_", "BINDGEN_EXTRA_CLANG_ARGS")):
            env.pop(key)
    for key in ("LIBRARY_PATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "CPPFLAGS",
                "CFLAGS", "CXXFLAGS", "LDFLAGS", "CMAKE_PREFIX_PATH", "CMAKE_INCLUDE_PATH",
                "CMAKE_LIBRARY_PATH", "CMAKE_TOOLCHAIN_FILE", "CARGO_BUILD_TARGET"):
        env.pop(key, None)
    work = Path(root).resolve() / "dist/build/windows"
    env.update({
        "PATH": config["toolPath"] + os.pathsep + env["PATH"],
        # Preserve native CC/CXX for build scripts and procedural macros.
        f"CC_{TARGET_KEY}": config["cc"],
        f"CXX_{TARGET_KEY}": config["cxx"],
        f"AR_{TARGET_KEY}": config["ar"],
        f"RC_{TARGET_KEY}": config["rc"],
        f"CXXSTDLIB_{TARGET_KEY}": "stdc++",
        f"CFLAGS_{TARGET_KEY}": config["cflags"],
        f"CXXFLAGS_{TARGET_KEY}": config["cflags"] + " -DU_STATIC_IMPLEMENTATION",
        "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER": config["cc"],
        f"BINDGEN_EXTRA_CLANG_ARGS_{TARGET_KEY}": config["bindgenArgs"],
        f"CMAKE_TOOLCHAIN_FILE_{TARGET_KEY}": str(work / "toolchain.cmake"),
        "CMAKE_PREFIX_PATH": config["cmakePrefixPath"],
        "CMAKE_GENERATOR": "Ninja",
        "PKG_CONFIG_PATH": environ.get("PKG_CONFIG_PATH", ""),
        "PKG_CONFIG_LIBDIR": environ.get("PKG_CONFIG_LIBDIR", ""),
        f"PKG_CONFIG_PATH_{TARGET_KEY}": config["pkgConfigPath"],
        f"PKG_CONFIG_LIBDIR_{TARGET_KEY}": config["pkgConfigPath"],
        f"PKG_CONFIG_SYSROOT_DIR_{TARGET_KEY}": "/",
        "PKG_CONFIG_ALLOW_CROSS": "1",
        "PKG_CONFIG_ALL_STATIC": "1",
        "GDAL_HOME": config["gdal"],
        "GDAL_STATIC": "1",
        "MLN_STATIC_DEPS": "1",
        "MLN_PRECOMPILE": "0",
        "MLN_CMAKE_PYTHON_EXECUTABLE": os.path.realpath(os.sys.executable),
        "CARGO_TARGET_DIR": str(work / "target"),
        "RUSTC_WRAPPER": "",
        "RUSTC_WORKSPACE_WRAPPER": "",
        "MLN_CMAKE_CXX_LAUNCHER": "",
        "CMAKE_C_COMPILER_LAUNCHER": "",
        "CMAKE_CXX_COMPILER_LAUNCHER": "",
    })
    # Hyphenated overrides take priority over underscored ones.
    for name in ("CC", "CXX", "AR", "RC", "CXXSTDLIB", "CFLAGS", "CXXFLAGS",
                 "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "PKG_CONFIG_SYSROOT_DIR",
                 "BINDGEN_EXTRA_CLANG_ARGS", "CMAKE_TOOLCHAIN_FILE"):
        env[f"{name}_{TARGET}"] = env[f"{name}_{TARGET_KEY}"]
    flags = (environ["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
             if environ.get("CARGO_ENCODED_RUSTFLAGS") else shlex.split(environ.get("RUSTFLAGS", "")))
    flags.extend(["-Ctarget-feature=+crt-static", "-Clink-arg=-static",
                  "-Clink-arg=-static-libgcc", "-Clink-arg=-static-libstdc++",
                                    "-Clink-arg=-lmcfgthread", "-Clink-arg=-lntdll"])
    flags.extend(f"-Lnative={path}" for path in config["libraryPath"].split(":") if path)
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    env.pop("RUSTFLAGS", None)
    return env


def cmake_quote(value):
    return '"' + value.replace("\\", "/").replace('"', '\\"').replace("$", "\\$") + '"'


def prepare_build(config, env):
    work = Path(env["CARGO_TARGET_DIR"]).parent
    work.mkdir(parents=True, exist_ok=True)
    (work / "toolchain.cmake").write_text(
        "set(CMAKE_SYSTEM_NAME Windows)\n"
        "set(CMAKE_SYSTEM_PROCESSOR x86_64)\n"
        "set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)\n"
        + "".join(f"set({name} {cmake_quote(config[key])})\n" for name, key in (
            ("CMAKE_C_COMPILER", "cc"), ("CMAKE_CXX_COMPILER", "cxx"),
            ("CMAKE_AR", "ar"), ("CMAKE_RC_COMPILER", "rc"),
        ))
        + "set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)\n"
        + f"set(ENV{{PKG_CONFIG_PATH}} {cmake_quote(config['pkgConfigPath'])})\n"
        + f"set(ENV{{PKG_CONFIG_LIBDIR}} {cmake_quote(config['pkgConfigPath'])})\n"
        + "set(ENV{PKG_CONFIG_SYSROOT_DIR} \"/\")\n"
    )
    if not (Path(config["gdal"]) / "lib/libgdal.a").is_file():
        raise ValueError("MinGW GDAL static archive is missing")
    for key in ("cc", "cxx", "ar", "rc", "objdump", "magick"):
        if not Path(config[key]).is_file():
            raise ValueError(f"MinGW tool is missing: {config[key]}")


def prepare_resources(config, env, root):
    work = Path(env["CARGO_TARGET_DIR"]).parent
    work.mkdir(parents=True, exist_ok=True)
    icon = work / "map.ico"
    resource = work / "map-icon.o"
    subprocess.run([
        config["magick"], str(Path(root) / "data/icon.png"),
        "-define", "icon:auto-resize=256,128,64,48,32,16", str(icon),
    ], env=env, check=True)
    subprocess.run([
        config["rc"], "--input", str(Path(root) / "dist/windows.rc"),
        "--input-format=rc", "--output-format=coff", "--target=pe-x86-64",
        "--include-dir", str(work), "--output", str(resource),
    ], env=env, check=True)
    return resource


def build_command(config, resource):
    return [
        "cargo", "--config", "paths=" + json.dumps([config["gpuiWindows"], config["gpuiPreWindows"]]),
        # Select GPUI's runtime compiler, patched to use embedded, optimized HLSL.
        "--config", "profile.release.package.gpui-fast-windows.debug-assertions=true",
        "--config", "profile.release.package.gpui-pre-windows.debug-assertions=true",
        "rustc", "--release", "--locked", "--bin", "map", "--target", TARGET,
        # Keep later unqualified -lstdc++ requests from extracting DLL import
        # thunks for symbols first referenced by GDAL and its native dependencies.
        "--", "-l", "static:+whole-archive=stdc++", "-C", f"link-arg={resource}",
        # HDF5 is linked after MapLibre's bundled zlib; resolve it again here.
        # The C++ thread runtime must precede its Windows API import libraries.
        "-C", "link-arg=-lz", "-C", "link-arg=-lmcfgthread",
        "-C", "link-arg=-lkernel32", "-C", "link-arg=-lntdll",
    ]


def package_executable(binary, output, config, env):
    result = subprocess.run(
        [config["objdump"], "-p", str(binary)], env=env,
        check=True, stdout=subprocess.PIPE, text=True,
    )
    if "pei-x86-64" not in result.stdout:
        raise ValueError("The built executable is not a Windows x86_64 PE image")

    # Root resource types 3 and 14 are RT_ICON and RT_GROUP_ICON. Restrict
    # matches to the root so nested icon IDs cannot masquerade as resource types.
    resource_types = {
        int(match.group(1), 16)
        for match in re.finditer(r"^[0-9a-fA-F]+ {3}Entry: ID: 0x([0-9a-fA-F]+),", result.stdout, re.MULTILINE)
    }
    if not {3, 14}.issubset(resource_types):
        raise ValueError("The Windows executable is missing embedded icon resources")
    # Do not silently distribute a binary requiring MinGW or geospatial DLLs.
    imports = [line.split(":", 1)[1].strip().lower()
               for line in result.stdout.splitlines() if "DLL Name:" in line]
    # GPUI's jump-list code uses the Windows system ICU API (Windows 10 1703+),
    # independently of MapLibre's bundled ICU. Versioned external ICU stays banned.
    non_system = [name for name in imports if name != "icuuc.dll" and name.startswith((
        "libgcc", "libstdc++", "libwinpthread", "libmcfgthread", "libgdal", "gdal",
        "libproj", "proj", "libssl", "libcrypto", "libcurl", "libpng", "libjpeg",
        "libwebp", "libuv", "zlib", "icu",
    ))]
    if non_system:
        raise ValueError("Unexpected runtime DLL dependencies: " + ", ".join(non_system))
    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, output)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="realize and check release inputs without building Map")
    args = parser.parse_args()
    try:
        config = load_config(PROJECT_ROOT, tool_environment(os.environ))
        env = package_environment(config, os.environ, PROJECT_ROOT)
        prepare_build(config, env)
        resource = prepare_resources(config, env, PROJECT_ROOT)
        if args.check:
            for name in (f"CC_{TARGET_KEY}", f"CXX_{TARGET_KEY}", "GDAL_HOME", "CARGO_TARGET_DIR"):
                print(f"{name}={env[name]}")
            print(shlex.join(build_command(config, resource)))
            return
        command = build_command(config, resource)
        print(shlex.join(command), flush=True)
        subprocess.run(command, cwd=PROJECT_ROOT, env=env, check=True)
        binary = Path(env["CARGO_TARGET_DIR"]) / TARGET / "release/map.exe"
        output = PROJECT_ROOT / "dist/build/devenv/release/bundle/windows/Map.exe"
        print(f"Created {package_executable(binary, output, config, env)}")
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Windows packaging failed: {error}\n")


if __name__ == "__main__":
    main()
