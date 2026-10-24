"""Package an already-built portable Linux binary with linuxdeploy/AppImageKit."""

import os
import re
import shlex
import shutil
import stat
import subprocess
from pathlib import Path

# These are supplied by the host glibc, not by the AppDir. In particular, C++,
# graphics loaders, X11 and Wayland are deliberately NOT system exceptions.
SYSTEM_SONAMES = frozenset({
    "libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1",
    "libresolv.so.2", "libutil.so.1", "libanl.so.1", "libBrokenLocale.so.1",
    "ld-linux-x86-64.so.2",
})
GPU_DRIVERS = (
    "*_dri.so", "libvulkan_*.so*", "libnvidia*.so*", "libcuda.so*",
    "libGLX_mesa.so*", "libEGL_mesa.so*", "libGLX_nvidia.so*", "libEGL_nvidia.so*",
    "libgallium*.so*",
)
GLIBC_LIMIT = (2, 28)


def run(command, root, env, timeout=60):
    command = [str(part) for part in command]
    print(shlex.join(command), flush=True)
    try:
        result = subprocess.run(command, cwd=root, env=env, capture_output=True,
                                text=True, check=False, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"Timed out after {timeout}s: {shlex.join(command)}") from error
    if result.returncode:
        raise RuntimeError(f"{shlex.join(command)} failed ({result.returncode}):\n"
                           f"{result.stdout}\n{result.stderr}")
    return result.stdout


def tool_environment(env):
    """Do not inherit compilation wrappers or runtime injection from devenv."""
    clean = {key: value for key, value in env.items()
             if "SCCACHE" not in key.upper() and "sccache" not in value.lower()}
    for key in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "LD_PRELOAD", "LD_LIBRARY_PATH"):
        clean.pop(key, None)
    clean["PATH"] = os.pathsep.join(
        part for part in env.get("PATH", os.defpath).split(os.pathsep)
        if part and "sccache" not in part.lower()
    )
    clean.update({"LC_ALL": "C", "ARCH": "x86_64"})
    return clean


def runtime_library_dirs(closure):
    directories = []
    for line in Path(closure).read_text().splitlines():
        if not line:
            continue
        package = Path(line)
        if not package.is_absolute() or not str(package).startswith("/nix/store/"):
            raise ValueError(f"Invalid runtimeClosure store path: {line!r}")
        for suffix in ("lib", "lib64", "lib/x86_64-linux-gnu"):
            directory = package / suffix
            if directory.is_dir():
                directories.append(str(directory))
    return list(dict.fromkeys(directories))


def parse_elf(output):
    """Parse locale-independent, wide readelf header/dynamic/version output."""
    if not re.search(r"Class:\s+ELF64\b", output) or not re.search(
        r"Machine:\s+Advanced Micro Devices X86-64\b", output
    ):
        raise ValueError("Expected an x86_64 ELF64 file")
    return {
        "needed": re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", output),
        "sonames": re.findall(r"\(SONAME\).*?\[([^\]]+)\]", output),
        "rpaths": [entry for value in re.findall(
            r"\((?:RPATH|RUNPATH)\).*?\[([^\]]*)\]", output
        ) for entry in value.split(":")],
        "interpreters": re.findall(r"\[Requesting program interpreter:\s*([^\]]+)\]", output),
        "glibc": set(re.findall(r"\bGLIBC_([0-9]+(?:\.[0-9]+)+|PRIVATE|ABI_[A-Za-z0-9_]+)\b", output)),
    }


def check_elf(info, path, *, relocated=True):
    for version in info["glibc"]:
        if not re.fullmatch(r"[0-9]+(?:\.[0-9]+)+", version) or tuple(
            int(part) for part in version.split(".")
        ) > GLIBC_LIMIT:
            raise ValueError(f"{path}: unsupported GLIBC_{version}; maximum is GLIBC_2.28")
    for name in info["needed"] + info["sonames"]:
        if "/" in name:
            raise ValueError(f"{path}: nonportable absolute/path-based ELF dependency: {name}")
    for interpreter in info["interpreters"]:
        if interpreter != "/lib64/ld-linux-x86-64.so.2":
            raise ValueError(f"{path}: nonportable ELF interpreter: {interpreter}")
    if relocated:
        for rpath in info["rpaths"]:
            if not (rpath == "$ORIGIN" or rpath.startswith("$ORIGIN/")):
                raise ValueError(f"{path}: nonportable RPATH: {rpath!r}")


def read_elf(path, tools, root, env):
    return parse_elf(run([tools["readelf"], "--wide", "--file-header", "--program-headers",
                          "--dynamic", "--version-info", path], root, env))


def elf_files(appdir):
    files = []
    for path in sorted(appdir.rglob("*")):
        if path.is_symlink():
            if not path.resolve().is_relative_to(appdir.resolve()) or not path.exists():
                raise ValueError(f"AppDir contains an external or broken symlink: {path}")
        elif path.is_file():
            with path.open("rb") as source:
                if source.read(4) == b"\x7fELF":
                    files.append(path)
    return files


def audit_appdir(appdir, tools, root, env):
    """Relocate collected libraries; do not discover or copy dependencies ourselves."""
    files = elf_files(appdir)
    libdir = appdir / "usr/lib"
    for path in files:
        info = read_elf(path, tools, root, env)
        check_elf(info, path, relocated=False)
        for name in [path.name, *info["sonames"]]:
            if name in SYSTEM_SONAMES or any(Path(name).match(pattern) for pattern in GPU_DRIVERS):
                raise ValueError(f"Must not bundle host glibc/GPU driver: {path} ({name})")
        # linuxdeploy uses usr/lib. Also retain each object's own directory for
        # SONAME symlinks and any colocated dependencies it deployed.
        relative = os.path.relpath(libdir, path.parent)
        rpath = "$ORIGIN" if relative == "." else f"$ORIGIN:$ORIGIN/{relative}"
        if info["needed"] or info["rpaths"]:
            run([tools["patchelf"], "--set-rpath", rpath, path], root, env)

    for path in files:
        info = read_elf(path, tools, root, env)
        check_elf(info, path)
        search = [Path(entry.replace("$ORIGIN", str(path.parent))).resolve()
                  for entry in info["rpaths"]]

        for directory in search:
            if not directory.is_relative_to(appdir.resolve()):
                raise ValueError(f"{path}: RPATH escapes the AppDir: {directory}")
        for needed in info["needed"]:
            if needed in SYSTEM_SONAMES:
                continue
            candidates = [directory / needed for directory in search]
            if not any(candidate.is_file() and candidate.resolve() in files for candidate in candidates):
                raise ValueError(f"{path}: dependency not bundled/resolvable: {needed}")


def build_appimage(binary: Path, config: dict, root: Path, env: dict) -> Path:
    """Collect with linuxdeploy, audit the AppDir, then run wrapped AppImageKit 12."""
    root = Path(root).resolve()
    binary = Path(binary).resolve()
    if config["arch"] != "x86_64" or str(config["glibc"]) != "2.28":
        raise ValueError("Only x86_64 / glibc 2.28 AppImages are supported")
    tools = config["tools"]
    for name in ("linuxdeploy", "appimagetool", "patchelf", "readelf", "magick"):
        if not Path(tools[name]).is_absolute() or not os.access(tools[name], os.X_OK):
            raise ValueError(f"Missing absolute executable tool path: {name}={tools[name]}")
    clean = tool_environment(env)
    check_elf(read_elf(binary, tools, root, clean), binary, relocated=False)
    collector_env = {**clean, "LD_LIBRARY_PATH": os.pathsep.join(
        runtime_library_dirs(config["runtimeClosure"])
    )}
    appdir = root / "dist/build/appimage/Map.AppDir"
    output = root / "dist/build/devenv/release/bundle/appimage/Map-x86_64.AppImage"
    if appdir.is_symlink():
        raise ValueError(f"Refusing to replace an AppDir symlink: {appdir}")
    if appdir.exists():
        # copytree preserves Nix's read-only directory modes, including after failures.
        for directory, _, _ in os.walk(appdir):
            Path(directory).chmod(0o755)
        shutil.rmtree(appdir)
    appdir.mkdir(parents=True, mode=0o755)
    appdir.chmod(0o755)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.unlink(missing_ok=True)
    share = appdir / "usr/share"
    icon = share / "icons/hicolor/256x256/apps/map.png"
    icon.parent.mkdir(parents=True)
    run([tools["magick"], root / "data/icon.png", "-resize", "256x256", icon], root, clean)
    certificates = share / "map/certs/ca-bundle.crt"
    certificates.parent.mkdir(parents=True)
    ca = Path(config["caBundle"])
    if b"-----BEGIN CERTIFICATE-----" not in ca.read_bytes():
        raise ValueError(f"CA bundle is not PEM: {ca}")
    shutil.copyfile(ca, certificates)
    for source, destination in (
        (Path(config["xkbData"]), share / "X11/xkb"),
        (Path(config["composeData"]), share / "X11/locale"),
    ):
        shutil.copytree(source, destination)

    command = [tools["linuxdeploy"], "--appdir", appdir, "--executable", binary,
               "--desktop-file", root / "dist/map.desktop", "--icon-file", icon]
    for library in config["runtimeLibraries"]:
        command.extend(["--library", library])
    for pattern in [*sorted(SYSTEM_SONAMES), *GPU_DRIVERS]:
        command.extend(["--exclude-library", pattern])
    run(command, root, collector_env, timeout=600)
    # linuxdeploy may symlink AppRun to usr/bin/map: never overwrite its target.
    (appdir / "AppRun").unlink(missing_ok=True)
    shutil.copyfile(root / "dist/AppRun", appdir / "AppRun")
    (appdir / "AppRun").chmod(0o755)
    executable = appdir / "usr/bin/map"
    if not executable.is_file():
        raise ValueError(f"linuxdeploy did not install {executable}")
    audit_appdir(appdir, tools, root, clean)
    if stat.S_IMODE(appdir.stat().st_mode) != 0o755:
        raise ValueError(f"AppDir root must have mode 755: {appdir}")
    # This is Nix's wrapType2 entry point, not a raw AppImage: no FUSE or
    # --appimage-extract-and-run bootstrap is needed here.
    run([tools["appimagetool"], "--runtime-file", config["runtime"], appdir, output],
        root, clean, timeout=600)
    output.chmod(0o755)
    if stat.S_IMODE(output.stat().st_mode) != 0o755:
        raise ValueError(f"AppImage must have mode 755: {output}")
    runtime = read_elf(output, tools, root, clean)
    check_elf(runtime, output)
    for needed in runtime["needed"]:
        if needed not in SYSTEM_SONAMES:
            raise ValueError(f"AppImage runtime needs a non-glibc host library: {needed}")
    return output
