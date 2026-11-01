#!/usr/bin/env python3
"""Build pinned official FFI with Map's native extensions, without resetting edits.

Prints the absolute MAPLIBRE_NATIVE_FFI_LOCAL_BUILD export after installation.
Use --prepare-only to fetch/patch sources without requiring native build tools.
"""

import argparse
import hashlib
import json
import os
import platform
import shlex
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REVISION = "6718eff7beeb927c6c629b4da617338b0b8fab03"
REPOSITORY = "https://github.com/maplibre/maplibre-native-ffi.git"
DESCRIPTOR = "share/maplibre-native-c/artifact.json"
EXTENSIONS = ("maplibre-custom-layer", "maplibre-raster-compositor")
COMPOSITOR_SYMBOLS = tuple(f"map_compositor_{name}" for name in (
    "new", "prepare", "render", "deinitialize", "context_lost", "delete",
))
TARGETS = {
    "x86_64-unknown-linux-gnu": "linux-gnu-x64-vulkan",
    "aarch64-unknown-linux-gnu": "linux-gnu-arm64-vulkan",
    "x86_64-apple-darwin": "macos-x64-metal",
    "aarch64-apple-darwin": "macos-arm64-metal",
    "x86_64-pc-windows-msvc": "windows-x64-vulkan",
    "aarch64-pc-windows-msvc": "windows-arm64-vulkan",
}


def run(command, cwd, **kwargs):
    return subprocess.run(command, cwd=cwd, check=True, **kwargs)


def git_output(destination, *args):
    return run(["git", *args], destination, capture_output=True, text=True).stdout.strip()


def repositories(destination):
    nested = git_output(destination, "submodule", "foreach", "--quiet", "--recursive",
                        "git rev-parse --show-toplevel")
    return [destination, *(Path(line) for line in nested.splitlines())]


def worktree_state(destination):
    """Include staged, unstaged and nonignored untracked edits in every submodule."""
    result = {}
    for repo in repositories(destination):
        digest = hashlib.sha256()
        digest.update(git_output(repo, "diff", "--binary", "HEAD").encode())
        digest.update(git_output(repo, "diff", "--binary", "--cached").encode())
        untracked = git_output(repo, "ls-files", "--others", "--exclude-standard", "-z")
        for name in filter(None, untracked.split("\0")):
            path = repo / name
            digest.update(name.encode())
            content = os.readlink(path).encode() if path.is_symlink() else path.read_bytes()
            digest.update(content)
        result[str(repo.relative_to(destination))] = {
            "head": git_output(repo, "rev-parse", "HEAD"),
            "status": git_output(repo, "status", "--porcelain", "--untracked-files=all"),
            "diff": digest.hexdigest(),
        }
    return result


def read_record(destination):
    path = destination.parent / f"{destination.name}-map-prepared.json"
    return json.loads(path.read_text()) if path.exists() else None


def require_managed_or_clean(destination, record):
    state = worktree_state(destination)
    if record is not None:
        if state != record["state"]:
            raise RuntimeError(f"Local modifications in {destination}; refusing to reset them")
        return
    dirty = [name for name, entry in state.items() if entry["status"]]
    if dirty:
        raise RuntimeError(f"Unrecorded modifications in {destination}: {dirty}; use a new checkout")


def checkout(destination):
    if not destination.exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", "--no-recurse-submodules", REPOSITORY, str(destination)], ROOT)
    record = read_record(destination)
    require_managed_or_clean(destination, record)
    if git_output(destination, "rev-parse", "HEAD") == REVISION:
        return record
    if record:
        raise RuntimeError("Prepared checkout has a different revision; use a new checkout")
    found = subprocess.run(["git", "cat-file", "-e", f"{REVISION}^{{commit}}"],
                           cwd=destination, capture_output=True, check=False)
    if found.returncode:
        run(["git", "fetch", "origin", REVISION], destination)
    run(["git", "-c", "submodule.recurse=false", "checkout", "--detach", REVISION], destination)
    return None


def extension_files(root=ROOT):
    hooks = [root / "crates" / name / "cmake/inject.cmake" for name in EXTENSIONS]
    patches = sorted((root / "crates/maplibre-custom-layer/patches").glob("*.patch"))
    if not patches:
        raise RuntimeError("Missing custom-layer core patch")
    return hooks, patches


def preparation_fingerprint(destination, patches):
    digest = hashlib.sha256(REVISION.encode())
    official = sorted((destination / "patches/maplibre-native").glob("*.patch"))
    for path in [destination / "tools/deps/sync-submodules", *official, *patches]:
        digest.update(str(path).encode())
        digest.update(path.read_bytes())
    return digest.hexdigest()


def apply_patch(core, patch):
    reverse = subprocess.run(["git", "apply", "--reverse", "--check", str(patch)],
                             cwd=core, capture_output=True, check=False)
    if reverse.returncode == 0:
        return
    run(["git", "apply", "--check", str(patch)], core)
    run(["git", "apply", str(patch)], core)


def prepare(destination, patches):
    record = checkout(destination)
    fingerprint = preparation_fingerprint(destination, patches)
    if record:
        if record["fingerprint"] != fingerprint:
            raise RuntimeError("Preparation patches changed; use a new checkout to preserve existing edits")
        return
    # Upstream uses --force internally. Only call it after checking all initialized
    # submodules, and never repeat it over our additional core patch or user edits.
    run(["bash", "tools/deps/sync-submodules"], destination)
    core = destination / "third_party/maplibre-native"
    for patch in patches:
        apply_patch(core, patch)
    record = {"fingerprint": fingerprint, "state": worktree_state(destination)}
    path = destination.parent / f"{destination.name}-map-prepared.json"
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(record, indent=2) + "\n")
    temporary.replace(path)


def host_target():
    system = platform.system()
    architecture = {"x86_64": "x86_64", "AMD64": "x86_64", "arm64": "aarch64",
                    "aarch64": "aarch64"}.get(platform.machine())
    suffix = {"Linux": "unknown-linux-gnu", "Darwin": "apple-darwin",
              "Windows": "pc-windows-msvc"}.get(system)
    target = f"{architecture}-{suffix}"
    if target not in TARGETS:
        raise RuntimeError(f"Unsupported native host: {system}/{platform.machine()}")
    return target


def write_injection(build_dir, hooks):
    for hook in hooks:
        if not hook.is_file():
            raise RuntimeError(f"Missing extension injection hook: {hook}")
    build_dir.mkdir(parents=True, exist_ok=True)
    path = build_dir / "map-extensions.cmake"
    # Bracket arguments preserve spaces, backslashes and CMake variable characters.
    includes = [f"include([==[{hook.as_posix()}]==])" for hook in hooks]
    # The official Zig toolchain selects Rust's ld.lld. Nix's wrapper currently
    # contains unresolved @mktemp@ placeholders; use its real linker for the
    # relocatable complete-static archive step, without changing host/GPUI linking.
    linker = [
        'if(CMAKE_HOST_SYSTEM_NAME STREQUAL "Linux")',
        '  get_filename_component(map_linker_dir "${CMAKE_LINKER}" DIRECTORY)',
        '  get_filename_component(map_tools_dir "${map_linker_dir}" DIRECTORY)',
        '  set(map_real_linker "${map_tools_dir}/gcc-ld-unwrapped/ld.lld")',
        '  if(EXISTS "${map_real_linker}")',
        '    set(CMAKE_LINKER "${map_real_linker}" CACHE FILEPATH "Native archive linker" FORCE)',
        '    set(CMAKE_LINKER "${map_real_linker}")',
        '  endif()',
        'endif()',
    ]
    path.write_text("\n".join([*includes, *linker]) + "\n")
    return path


def validate_install(install_dir, target, preset):
    descriptor = json.loads((install_dir / DESCRIPTOR).read_text())
    expected = (REVISION, target, preset.rsplit("-", 1)[1])
    actual = (descriptor["commit"], descriptor["platform"]["rustTarget"], descriptor["backend"])
    if actual != expected:
        raise RuntimeError(f"Installed artifact mismatch: expected {expected}, found {actual}")
    archive = "maplibre-native-c-static.lib" if target.endswith("windows-msvc") else "libmaplibre-native-c.a"
    if not (install_dir / "lib" / archive).is_file():
        raise RuntimeError(f"Missing complete static FFI archive in {install_dir / 'lib'}")


def finalize_extensions(build_dir, target):
    if "linux" not in target:
        return
    # Upstream's ELF complete archive localizes everything outside mln_*. The
    # compositor has its own C ABI; restore only those entry points, not C++/core
    # internals. This changes generated artifacts, never the prepared sources.
    cache = (build_dir / "CMakeCache.txt").read_text().splitlines()
    objcopy = next(line.split("=", 1)[1] for line in cache if line.startswith("CMAKE_OBJCOPY:FILEPATH="))
    archive = build_dir / "maplibre_native_c_static-complete-static/libmaplibre-native-c.a"
    flags = [f"--globalize-symbol={name}" for name in COMPOSITOR_SYMBOLS]
    run([objcopy, *flags, str(archive)], build_dir)
    nm = str(Path(objcopy).with_name("llvm-nm"))
    symbols = archive_symbols(nm, archive, build_dir)
    required = {"mln_custom_layer_add", "mln_custom_layer_vulkan_context", *COMPOSITOR_SYMBOLS}
    missing = required - symbols
    if missing:
        raise RuntimeError(f"Native archive lacks extension entry points: {sorted(missing)}")


def archive_symbols(nm, archive, build_dir):
    output = run([nm, "--defined-only", "--extern-only", str(archive)], build_dir,
                 capture_output=True, text=True).stdout
    return {line.split()[-1] for line in output.splitlines() if line.strip()}


def build(destination, build_dir, install_dir, target, hooks, jobs, cmake_args):
    preset = TARGETS[target]
    injection = write_injection(build_dir, hooks)
    run(["cmake", "--preset", preset, "-B", str(build_dir),
         f"-DCMAKE_INSTALL_PREFIX={install_dir}", "-DBUILD_TESTING=OFF",
         *cmake_args, f"-DCMAKE_PROJECT_INCLUDE={injection}"], destination)
    run(["cmake", "--build", str(build_dir), "--config", "Release",
         "--parallel", str(jobs)], destination)
    finalize_extensions(build_dir, target)
    run(["cmake", "--install", str(build_dir), "--config", "Release"], destination)
    validate_install(install_dir, target, preset)
    print(f"export MAPLIBRE_NATIVE_FFI_LOCAL_BUILD={shlex.quote(str(install_dir))}", flush=True)


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", nargs="?", type=Path, default=ROOT / ".devenv/maplibre-native-ffi-map")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--install-dir", type=Path)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--cmake-arg", action="append", default=[],
                        help="Extra CMake option; use --cmake-arg=-DNAME=value")
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    return args


def main():
    args = arguments()
    target = args.target or host_target()
    hooks, patches = extension_files()
    destination = args.destination.resolve()
    build_dir = (args.build_dir or ROOT / ".devenv/maplibre-native-ffi-build" / TARGETS[target]).resolve()
    install_dir = (args.install_dir or build_dir / "install").resolve()
    if build_dir == destination or destination.is_relative_to(build_dir):
        raise RuntimeError("Build directory must not contain or replace the source checkout")
    prepare(destination, patches)
    if not args.prepare_only:
        build(destination, build_dir, install_dir, target, hooks, args.jobs, args.cmake_arg)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        sys.exit(f"Could not prepare MapLibre Native FFI: {error}")
