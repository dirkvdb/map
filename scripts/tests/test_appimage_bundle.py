"""Offline packaging regressions: python3 -B -m unittest discover -s scripts/tests -p test_appimage_bundle.py -v."""

import contextlib
import importlib
import io
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
packaging = importlib.import_module("scripts.appimage_bundle")

ROOT = Path(__file__).resolve().parents[2]
HEADER = "ELF Header:\n  Class: ELF64\n  Machine: Advanced Micro Devices X86-64\n"
DYNAMIC = """
  [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]
  0x0000000000000001 (NEEDED) Shared library: [libc.so.6]
  0x0000000000000001 (NEEDED) Shared library: [libxkbcommon.so.0]
  0x000000000000001d (RUNPATH) Library runpath: [$ORIGIN/../lib:$ORIGIN]
Version needs section '.gnu.version_r' contains 1 entry:
  0x0010: Name: GLIBC_2.2.5 Flags: none Version: 3
  0x0020: Name: GLIBC_2.28 Flags: none Version: 2
"""


def elf_info(**updates):
    info = packaging.parse_elf(HEADER)
    info.update(updates)
    return info


class AppRunTests(unittest.TestCase):
    KEYS = ("APPDIR", "CURL_CA_BUNDLE", "SSL_CERT_FILE", "LD_LIBRARY_PATH", "PATH",
            "XDG_DATA_DIRS", "XKB_CONFIG_ROOT", "XLOCALEDIR", "GDAL_DATA", "GDAL_DRIVER_PATH")

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="map appimage tests ")
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.appdir = self.work / "Map With Spaces.AppDir"
        (self.appdir / "usr/bin").mkdir(parents=True)
        self.launcher = self.appdir / "AppRun"
        source = (ROOT / "dist/AppRun").read_text()
        self.assertTrue(source.startswith("#!/bin/sh\n"))
        self.assertNotIn("/nix/", source)
        # Substitute only the standard host probe paths, not launcher logic, so
        # the real POSIX shell can test host selection/fallback on Nix and CI.
        host_paths = re.findall(r"/etc/[A-Za-z0-9/_.-]+", source)
        self.assertEqual(len(host_paths), 5)
        self.hosts = [self.work / f"host trust {number}.pem" for number in range(5)]
        for original, fixture in zip(host_paths, self.hosts):
            source = source.replace(original, f'"{fixture}"')
        self.launcher.write_text(source)
        self.launcher.chmod(0o755)
        self.fallback = self.appdir / "usr/share/map/certs/ca-bundle.crt"
        self.fallback.parent.mkdir(parents=True)
        self.fallback.write_text("packaged PEM fixture")
        executable = self.appdir / "usr/bin/map"
        executable.write_text("#!/bin/sh\n" + "printf '%s\\0' " + " ".join(
            f'"${{{key}:-}}"' for key in self.KEYS
        ) + '\nprintf \'%s\\0\' "$@"\n')
        executable.chmod(0o755)

    def launch(self, overrides=None, arguments=("argument with spaces", "--flag")):
        env = {"PATH": os.environ.get("PATH", os.defpath), **(overrides or {})}
        result = subprocess.run(["/bin/sh", str(self.launcher), *arguments],
                                cwd=self.work, env=env, capture_output=True, check=True, timeout=10)
        values = result.stdout.decode().split("\0")[:-1]
        self.assertEqual(values[len(self.KEYS):], list(arguments))
        return dict(zip(self.KEYS, values))

    def test_fallback_and_keyboard_data_paths_with_spaces(self):
        values = self.launch()
        self.assertEqual(values["SSL_CERT_FILE"], str(self.fallback))
        for key, suffix in (("XKB_CONFIG_ROOT", "usr/share/X11/xkb"),
                            ("XLOCALEDIR", "usr/share/X11/locale")):
            self.assertEqual(values[key], str(self.appdir / suffix))
        self.assertEqual(values["PATH"], os.environ.get("PATH", os.defpath))
        for key in ("APPDIR", "CURL_CA_BUNDLE", "LD_LIBRARY_PATH", "XDG_DATA_DIRS",
                    "GDAL_DATA", "GDAL_DRIVER_PATH"):
            with self.subTest(key=key):
                self.assertEqual(values[key], "")

    def test_first_readable_host_ca_wins(self):
        self.hosts[0].mkdir()  # Readable directory is not a CA file.
        self.hosts[1].write_text("first host PEM")
        self.hosts[2].write_text("second host PEM")
        values = self.launch()
        self.assertEqual(values["SSL_CERT_FILE"], str(self.hosts[1]))
        self.assertEqual(values["CURL_CA_BUNDLE"], "")

    def test_explicit_curl_takes_precedence_even_if_invalid(self):
        self.hosts[0].write_text("host PEM")
        # Use the unmodified production launcher for this precedence test.
        shutil.copyfile(ROOT / "dist/AppRun", self.launcher)
        values = self.launch({"CURL_CA_BUNDLE": "/missing/curl trust.pem",
                              "SSL_CERT_FILE": "/missing/SSL trust.pem"})
        self.assertEqual(values["CURL_CA_BUNDLE"], "/missing/curl trust.pem")
        self.assertEqual(values["SSL_CERT_FILE"], "/missing/curl trust.pem")

    def test_explicit_curl_supplies_ssl_cert_file(self):
        values = self.launch({"CURL_CA_BUNDLE": "/explicit curl.pem"})
        self.assertEqual(values["CURL_CA_BUNDLE"], "/explicit curl.pem")
        self.assertEqual(values["SSL_CERT_FILE"], "/explicit curl.pem")

    def test_explicit_ssl_is_preserved_without_host_fallback(self):
        self.hosts[0].write_text("host PEM")
        values = self.launch({"SSL_CERT_FILE": "/explicit ssl.pem", "CURL_CA_BUNDLE": ""})
        self.assertEqual(values["CURL_CA_BUNDLE"], "")
        self.assertEqual(values["SSL_CERT_FILE"], "/explicit ssl.pem")

    def test_empty_overrides_use_fallback(self):
        values = self.launch({"CURL_CA_BUNDLE": "", "SSL_CERT_FILE": ""})
        self.assertEqual(values["SSL_CERT_FILE"], str(self.fallback))
        self.assertEqual(values["CURL_CA_BUNDLE"], "")

    def test_leaves_unrelated_environment_unchanged(self):
        overrides = {"APPDIR": "/caller/appdir", "LD_LIBRARY_PATH": "/extra libs",
                     "XDG_DATA_DIRS": "/custom data", "GDAL_DATA": "/caller/gdal-data",
                     "GDAL_DRIVER_PATH": "/caller/plugins"}
        values = self.launch(overrides)
        for key, value in overrides.items():
            with self.subTest(key=key):
                self.assertEqual(values[key], value)


class ElfGuardTests(unittest.TestCase):
    def test_parses_wide_readelf_output_and_accepts_baseline(self):
        info = packaging.parse_elf(HEADER + DYNAMIC)
        self.assertEqual(info["needed"], ["libc.so.6", "libxkbcommon.so.0"])
        self.assertEqual(info["rpaths"], ["$ORIGIN/../lib", "$ORIGIN"])
        self.assertEqual(info["interpreters"], ["/lib64/ld-linux-x86-64.so.2"])
        self.assertEqual(info["glibc"], {"2.2.5", "2.28"})
        packaging.check_elf(info, "map")

    def test_rejects_newer_or_private_glibc_versions_numerically(self):
        for version in ("2.29", "2.100", "PRIVATE", "ABI_DT_RELR"):
            with self.subTest(version=version):
                info = packaging.parse_elf(HEADER + DYNAMIC + f"Name: GLIBC_{version}\n")
                with self.assertRaisesRegex(ValueError, "unsupported GLIBC_"):
                    packaging.check_elf(info, "map")

    def test_rejects_absolute_nix_needed_soname_and_interpreter(self):
        for output in (
            "(NEEDED) Shared library: [/nix/store/hash-lib/lib/libfoo.so.1]",
            "(SONAME) Library soname: [/nix/store/hash-lib/lib/libfoo.so.1]",
            "[Requesting program interpreter: /nix/store/hash-glibc/lib/ld-linux-x86-64.so.2]",
        ):
            with self.subTest(output=output), self.assertRaisesRegex(ValueError, "nonportable"):
                packaging.check_elf(packaging.parse_elf(HEADER + output), "map")

    def test_rejects_nix_absolute_and_empty_rpath_after_relocation(self):
        for rpath in ("/nix/store/hash/lib", "/usr/lib", "", "$ORIGIN:"):
            with self.subTest(rpath=rpath):
                info = packaging.parse_elf(HEADER + f"(RPATH) Library rpath: [{rpath}]")
                with self.assertRaisesRegex(ValueError, "nonportable RPATH"):
                    packaging.check_elf(info, "lib.so")
                packaging.check_elf(info, "lib.so", relocated=False)

    def test_rejects_wrong_or_missing_architecture(self):
        for output in ("", HEADER.replace("ELF64", "ELF32"),
                       HEADER.replace("Advanced Micro Devices X86-64", "AArch64")):
            with self.subTest(output=output), self.assertRaisesRegex(ValueError, "x86_64 ELF64"):
                packaging.parse_elf(output)

    def test_parses_soname_and_static_elf(self):
        info = packaging.parse_elf(HEADER + "(SONAME) Library soname: [libfoo.so.1]")
        self.assertEqual(info["sonames"], ["libfoo.so.1"])
        packaging.check_elf(packaging.parse_elf(HEADER + "There is no dynamic section in this file."), "static")


class PackagingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="map bundle ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.appdir = self.root / "Map.AppDir"
        (self.appdir / "usr/bin").mkdir(parents=True)
        (self.appdir / "usr/lib").mkdir()
        self.binary = self.appdir / "usr/bin/map"
        self.binary.write_bytes(b"\x7fELFfixture")

    def test_removes_sccache_and_loader_injection_without_mutating_caller(self):
        env = {"PATH": "/tools/bin:/nix/store/hash-sccache/bin:/usr/bin", "SCCACHE_DIR": "/cache",
               "RUSTC_WRAPPER": "sccache", "RUSTC_WORKSPACE_WRAPPER": "wrapper",
               "CC": "sccache clang", "LD_PRELOAD": "bad.so", "LD_LIBRARY_PATH": "/native/lib"}
        original = env.copy()
        clean = packaging.tool_environment(env)
        self.assertEqual(env, original)
        self.assertEqual(clean["PATH"], "/tools/bin:/usr/bin")
        self.assertEqual(clean["ARCH"], "x86_64")
        for key in env.keys() - {"PATH"}:
            self.assertNotIn(key, clean)

    def test_runtime_library_dirs_come_from_closure_not_inherited_environment(self):
        closure = self.root / "store-paths"
        closure.write_text("/nix/store/hash-ui\n/nix/store/hash-data\n/nix/store/hash-ui\n")
        with patch.object(Path, "is_dir", side_effect=lambda: False):
            self.assertEqual(packaging.runtime_library_dirs(closure), [])
        with patch.object(Path, "is_dir", return_value=True):
            directories = packaging.runtime_library_dirs(closure)
        self.assertEqual(len(directories), 6)
        self.assertEqual(directories[0], "/nix/store/hash-ui/lib")
        closure.write_text("relative/path\n")
        with self.assertRaisesRegex(ValueError, "store path"):
            packaging.runtime_library_dirs(closure)

    def test_audit_patches_rpath_and_rejects_missing_non_glibc_library(self):
        before = elf_info(needed=["libmissing.so.1"], rpaths=["/nix/store/hash/lib"])
        after = elf_info(needed=["libmissing.so.1"], rpaths=["$ORIGIN", "$ORIGIN/../lib"])
        with patch.object(packaging, "read_elf", side_effect=[before, after]), \
             patch.object(packaging, "run") as run, \
             self.assertRaisesRegex(ValueError, "not bundled/resolvable: libmissing.so.1"):
            packaging.audit_appdir(self.appdir, {"patchelf": "/tools/patchelf"}, self.root, {})
        self.assertEqual(run.call_args.args[0],
                         ["/tools/patchelf", "--set-rpath", "$ORIGIN:$ORIGIN/../lib", self.binary])

    def test_audit_accepts_bundled_soname_symlink_and_host_glibc(self):
        library = self.appdir / "usr/lib/libui.so.1.0"
        library.write_bytes(b"\x7fELFfixture")
        (library.parent / "libui.so.1").symlink_to(library.name)
        def read(path, *args):
            return elf_info(needed=["libui.so.1", "libc.so.6"] if path == self.binary else [],
                            rpaths=["$ORIGIN", "$ORIGIN/../lib"])
        with patch.object(packaging, "read_elf", side_effect=read), patch.object(packaging, "run"):
            packaging.audit_appdir(self.appdir, {"patchelf": "/tools/patchelf"}, self.root, {})

    def test_audit_rejects_bundled_glibc_and_host_gpu_drivers(self):
        for name in ("libc.so.6", "libvulkan_intel.so", "iris_dri.so", "libEGL_mesa.so.0"):
            with self.subTest(name=name):
                driver = self.appdir / "usr/lib" / name
                driver.write_bytes(b"\x7fELFfixture")
                with patch.object(packaging, "read_elf", return_value=elf_info()), \
                     self.assertRaisesRegex(ValueError, "Must not bundle"):
                    packaging.audit_appdir(self.appdir, {}, self.root, {})
                driver.unlink()

    def test_audit_rejects_external_symlinks(self):
        (self.appdir / "usr/lib/external").symlink_to(self.root)
        with self.assertRaisesRegex(ValueError, "external or broken symlink"):
            packaging.elf_files(self.appdir)

    def test_build_delegates_collection_and_image_generation_to_configured_tools(self):
        (self.root / "dist").mkdir()
        for name in ("AppRun", "map.desktop"):
            shutil.copyfile(ROOT / "dist" / name, self.root / "dist" / name)
        tools = {}
        for name in ("linuxdeploy", "appimagetool", "readelf", "patchelf", "magick"):
            path = self.root / name
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
            tools[name] = str(path)
        ca = self.root / "ca.pem"
        ca.write_text("-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----\n")
        for name in ("xkb", "compose"):
            directory = self.root / name
            directory.mkdir(parents=True)
            (directory / "data").write_text("fixture")
            directory.chmod(0o555)
        config = {"arch": "x86_64", "glibc": "2.28", "tools": tools, "caBundle": str(ca),
                  "xkbData": str(self.root / "xkb"), "composeData": str(self.root / "compose"),
                  "runtimeClosure": "unused", "runtime": "/portable/runtime",
                  "runtimeLibraries": ["/portable/lib/libui.so.1", "/portable/lib/libEGL.so.1"]}
        calls = []
        def run(command, root, env, timeout=60):
            calls.append((command, env, timeout))
            if command[0] == tools["magick"]:
                Path(command[-1]).write_bytes(b"PNG fixture")
            elif command[0] == tools["linuxdeploy"]:
                appdir = Path(command[command.index("--appdir") + 1])
                (appdir / "usr/bin").mkdir(parents=True)
                shutil.copyfile(self.binary, appdir / "usr/bin/map")
                (appdir / "AppRun").symlink_to("usr/bin/map")
            elif command[0] == tools["appimagetool"]:
                Path(command[-1]).write_bytes(b"\x7fELF AppImage fixture")
            return ""
        env = {"PATH": os.defpath, "RUSTC_WRAPPER": "sccache", "SCCACHE_DIR": "/cache"}
        with patch.object(packaging, "runtime_library_dirs", return_value=["/portable/lib"]), \
             patch.object(packaging, "run", side_effect=run), \
             patch.object(packaging, "read_elf", return_value=elf_info()):
            output = packaging.build_appimage(self.binary, config, self.root, env)
            # Rebuilding must also clean data copied from read-only Nix directories.
            output = packaging.build_appimage(self.binary, config, self.root, env)
        appdir = self.root / "dist/build/appimage/Map.AppDir"
        self.assertEqual(output, self.root / "dist/build/devenv/release/bundle/appimage/Map-x86_64.AppImage")
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE(appdir.stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE((appdir / "AppRun").stat().st_mode), 0o755)
        self.assertFalse((appdir / "AppRun").is_symlink())
        self.assertEqual((appdir / "usr/bin/map").read_bytes(), self.binary.read_bytes())
        for destination in ("usr/share/X11/xkb/data", "usr/share/X11/locale/data",
                            "usr/share/map/certs/ca-bundle.crt"):
            self.assertTrue((appdir / destination).is_file())
        self.assertFalse((appdir / "usr/share/gdal").exists())
        collector, collector_env, timeout = next(call for call in calls if call[0][0] == tools["linuxdeploy"])
        self.assertEqual(collector_env["LD_LIBRARY_PATH"], "/portable/lib")
        self.assertEqual(timeout, 600)
        self.assertEqual([collector[index + 1] for index, value in enumerate(collector) if value == "--library"],
                         config["runtimeLibraries"])
        for flag in ("--appdir", "--executable", "--desktop-file", "--icon-file"):
            self.assertIn(flag, collector)
        image_command = next(command for command, _, _ in calls if command[0] == tools["appimagetool"])
        self.assertEqual(image_command[1:3], ["--runtime-file", config["runtime"]])
        for command, tool_env, timeout in calls:
            self.assertNotIn("SCCACHE_DIR", tool_env)
            self.assertNotIn("RUSTC_WRAPPER", tool_env)
            if command[0] != tools["linuxdeploy"]:
                self.assertNotIn("LD_LIBRARY_PATH", tool_env)

    def test_subprocess_failure_and_timeout_are_bounded_and_actionable(self):
        with contextlib.redirect_stdout(io.StringIO()):
            with patch.object(subprocess, "run", side_effect=subprocess.TimeoutExpired("tool", 5)) as run:
                with self.assertRaisesRegex(RuntimeError, "Timed out after 5s"):
                    packaging.run(["/tool"], self.root, {}, timeout=5)
                self.assertEqual(run.call_args.kwargs["timeout"], 5)
            with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(
                ["/tool"], 1, "output", "failure detail"
            )), self.assertRaisesRegex(RuntimeError, "failure detail"):
                packaging.run(["/tool"], self.root, {})


if __name__ == "__main__":
    unittest.main()
