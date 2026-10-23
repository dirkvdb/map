#!/usr/bin/env python3
"""Build and verify Map's macOS app and DMG using devenv's static libraries."""

import argparse
import os
import platform
import plistlib
import re
import shlex
import subprocess
import sys
from pathlib import Path
from xml.parsers.expat import ExpatError

PROJECT_ROOT = Path(__file__).resolve().parent.parent
STATIC_PACKAGES = ("libpng", "libjpeg", "libwebp", "libcurl", "sqlite3", "zlib", "gdal")
SETUP_VARIABLES = (
    "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "GDAL_HOME", "MLN_LOCAL_REPOSITORY",
    "CARGO_TARGET_DIR", "CC", "CXX", "SDKROOT",
)
DEPENDENCY = re.compile(
    r"[ \t]+(.+?) \(compatibility version [0-9]+(?:\.[0-9]+)*, "
    r"current version [0-9]+(?:\.[0-9]+)*\)"
)


def capture(command, env, root):
    result = subprocess.run(command, env=env, cwd=root, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic output"
        raise RuntimeError(f"{' '.join(command)} failed (exit {result.returncode}): {detail}")
    return result.stdout.strip()


def package_environment(environ, root):
    if platform.system() != "Darwin":
        raise ValueError("macOS packaging requires Darwin")
    if sys.version_info < (3, 10):
        raise ValueError("macOS packaging requires Python 3.10 or newer; use devenv's Python")
    required = ("MAP_PACKAGE_PKG_CONFIG_PATH", "MAP_PACKAGE_GDAL_HOME", "MAP_PACKAGE_LIBRARY_PATH")
    missing = [name for name in required if not environ.get(name, "").strip()]
    if missing:
        raise ValueError("Missing devenv configuration: " + ", ".join(missing) + "; run just dmg")

    root = Path(root).resolve()
    env = dict(environ)
    env.update({
        "PKG_CONFIG_PATH": env["MAP_PACKAGE_PKG_CONFIG_PATH"],
        "PKG_CONFIG_LIBDIR": env["MAP_PACKAGE_PKG_CONFIG_PATH"],
        "PKG_CONFIG_ALL_STATIC": "1",
        "GDAL_HOME": env["MAP_PACKAGE_GDAL_HOME"],
        "GDAL_STATIC": "1",
        "MLN_STATIC_DEPS": "1",
        "CARGO_TARGET_DIR": str(root / "dist/build/devenv"),
        # Apple's tool directories contain an older Python unsuitable for WebGPU generation.
        "MLN_CMAKE_PYTHON_EXECUTABLE": str(Path(sys.executable).resolve()),
        # Stripped release proc-macros fail to load with this Rust/Apple linker combination.
        "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP": "none",
    })
    native_env = env.copy()
    native_env.pop("DEVELOPER_DIR", None)
    native_env.pop("SDKROOT", None)
    for name, command in (
        ("SDKROOT", ["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"]),
        ("DEVELOPER_DIR", ["/usr/bin/xcode-select", "-p"]),
        ("CC", ["/usr/bin/xcrun", "--find", "clang"]),
        ("CXX", ["/usr/bin/xcrun", "--find", "clang++"]),
        ("LD", ["/usr/bin/xcrun", "--find", "ld"]),
    ):
        path = capture(command, native_env, root)
        if not path or not Path(path).is_absolute() or len(path.splitlines()) != 1:
            raise ValueError(f"{' '.join(command)} returned an invalid {name} path: {path!r}")
        env[name] = path

    native_linker = env.pop("LD")
    for name in ("OBJC", "OBJCXX", "LIBRARY_PATH"):
        env.pop(name, None)
    native_bins = [str(Path(native_linker).parent), str(Path(env["CC"]).parent)]
    env["PATH"] = os.pathsep.join(dict.fromkeys([
        *native_bins, *env.get("PATH", os.defpath).split(os.pathsep),
    ]))
    version = capture(["rustc", "-vV"], env.copy(), root)
    hosts = [line.removeprefix("host: ").strip() for line in version.splitlines()
             if line.startswith("host: ")]
    if len(hosts) != 1 or hosts[0] not in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
        raise ValueError(f"rustc -vV returned an unsupported or missing macOS host: {version!r}")
    env["CARGO_TARGET_" + hosts[0].upper().replace("-", "_") + "_LINKER"] = env["CC"]
    if "CARGO_ENCODED_RUSTFLAGS" in env:
        flags = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f") if env["CARGO_ENCODED_RUSTFLAGS"] else []
    else:
        flags = shlex.split(env.get("RUSTFLAGS", ""))
    flags.extend(f"-Lnative={path}" for path in env["MAP_PACKAGE_LIBRARY_PATH"].split(":") if path)
    flags.append(f"-Clink-arg=-fuse-ld={native_linker}")
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    env.pop("RUSTFLAGS", None)
    return env


def check_environment(env, root):
    for name in SETUP_VARIABLES:
        print(f"{name}={env.get(name, '<unset>')}")
    for name in sorted(env):
        if name.startswith("CARGO_TARGET_") and name.endswith("_LINKER"):
            print(f"{name}={env[name]}")
    command = ["pkg-config", "--static", "--libs", *STATIC_PACKAGES]
    print(f"{' '.join(command)}:\n{capture(command, env, root)}")


def parse_dependencies(output, binary):
    dependencies = []
    header = re.compile(re.escape(binary) + r"(?: \(architecture [A-Za-z0-9_]+\))?:")
    have_header = False
    section_count = 0
    for number, line in enumerate(output.splitlines(), 1):
        if not line.strip():
            continue
        if header.fullmatch(line):
            if have_header and section_count == 0:
                raise ValueError("otool output contains a header without dependencies")
            have_header = True
            section_count = 0
            continue
        match = DEPENDENCY.fullmatch(line)
        if not have_header or match is None:
            raise ValueError(f"Malformed otool output at line {number}: {line!r}")
        dependencies.append(match.group(1))
        section_count += 1
    if not have_header or section_count == 0:
        raise ValueError("otool output has no dependencies")
    return dependencies


def check_dependencies(binary):
    result = subprocess.run(["/usr/bin/otool", "-L", binary], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic output"
        raise RuntimeError(f"otool failed (exit {result.returncode}): {detail}")
    offending = [path for path in parse_dependencies(result.stdout, binary)
                 if not path.startswith(("/usr/lib/", "/System/Library/"))]
    if offending:
        raise ValueError("Non-system dependencies:\n" + "\n".join(f"  {path}" for path in offending))


def check_bundle(bundle):
    contents = Path(bundle) / "Contents"
    info_path = contents / "Info.plist"
    try:
        with info_path.open("rb") as source:
            info = plistlib.load(source)
    except (ValueError, ExpatError) as error:
        raise ValueError(f"Invalid bundle Info.plist at {info_path}: {error}") from error
    if not isinstance(info, dict):
        raise ValueError(f"Bundle Info.plist must contain a dictionary: {info_path}")
    for key in ("CFBundleExecutable", "CFBundleIconFile"):
        if not isinstance(info.get(key), str) or not info[key].strip():
            raise ValueError(f"Bundle Info.plist requires a nonempty string for {key}: {info_path}")
    executable = contents / "MacOS" / info["CFBundleExecutable"]
    if not executable.is_file():
        raise ValueError(f"Bundle executable is missing or not a file: {executable}")
    icon = contents / "Resources" / info["CFBundleIconFile"]
    if not icon.suffix:
        icon = icon.with_suffix(".icns")
    if not icon.is_file():
        raise ValueError(f"Bundle icon is missing or not a file: {icon}")
    if icon.stat().st_size == 0:
        raise ValueError(f"Bundle icon is empty: {icon}")
    check_dependencies(str(executable))


def build_dmg(env, root):
    def run(*command):
        print(shlex.join(command), flush=True)
        subprocess.run(command, env=env, cwd=root, check=True)

    for size, name in ((512, "icon_512x512.png"), (1024, "icon_512x512@2x.png")):
        run("/usr/bin/sips", "-z", str(size), str(size), "data/icon.png", "--out", f"dist/{name}")
    run("cargo", "build", "--release", "--locked", "--bin", "map")
    release = Path(env["CARGO_TARGET_DIR"]) / "release"
    check_dependencies(str(release / "map"))
    subprocess.run(["cargo", "bundle", "--release", "--format", "osx"],
                   env={**env, "CARGO_BUNDLE_SKIP_BUILD": "1"}, cwd=root, check=True)
    bundles = release / "bundle"
    app = bundles / "osx/Map.app"
    check_bundle(app)
    run("/usr/bin/codesign", "--force", "--sign", "-", str(app))
    run("/usr/bin/codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app))
    run("/bin/ln", "-sfn", "/Applications", str(bundles / "osx/Applications"))
    (bundles / "dmg").mkdir(parents=True, exist_ok=True)
    dmg = str(bundles / "dmg/Map.dmg")
    run("/usr/bin/hdiutil", "create", "-ov", "-volname", "Map", "-srcfolder", str(bundles / "osx"),
        "-format", "UDZO", dmg)
    run("/usr/bin/hdiutil", "verify", dmg)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="check the build environment without building")
    args = parser.parse_args()
    try:
        env = package_environment(os.environ, PROJECT_ROOT)
        if args.check:
            check_environment(env, PROJECT_ROOT)
        else:
            build_dmg(env, PROJECT_ROOT)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"macOS packaging failed: {error}\n")


if __name__ == "__main__":
    main()
