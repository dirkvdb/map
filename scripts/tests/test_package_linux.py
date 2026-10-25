"""Offline Linux packaging regressions.

Run with python3 -B -m unittest discover -s scripts/tests -p test_package_linux.py -v.
"""

import contextlib
import io
import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import package_linux as packaging

ROOT = Path("/tmp/Map Project").resolve()
TARGET = "x86_64-unknown-linux-gnu"
CONFIG = {
    "target": TARGET,
    "hostCc": "/nix/store/native-toolchain/bin/cc",
    "cc": "/nix/store/portable-toolchain/bin/cc",
    "cxx": "/nix/store/portable-toolchain/bin/c++",
    "ar": "/nix/store/portable-toolchain/bin/ar",
    "pkgConfigPath": "/nix/store/portable-gdal/lib/pkgconfig:/nix/store/portable-ui/lib/pkgconfig",
    "libraryPath": "/nix/store/portable-gdal/lib:/nix/store/portable-ui/lib",
    "gdal": "/nix/store/portable-gdal",
    "glibc": "2.28",
    "cmakePrefixPath": "/nix/store/portable-gdal;/nix/store/portable-ui",
}


class PackageEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(patch.object(packaging.platform, "system", return_value="Linux"))
        self.enterContext(patch.object(packaging.platform, "machine", return_value="x86_64"))

    def test_linux_target_toolchain_and_portable_libraries(self):
        env = packaging.package_environment(CONFIG, {}, ROOT)
        work = ROOT / "dist/build/appimage"
        expected = {
            "CC": CONFIG["cc"],
            "CXX": CONFIG["cxx"],
            "AR": CONFIG["ar"],
            "CXXSTDLIB": "c++",
            "CFLAGS": "-target x86_64-linux-gnu.2.28",
            "CXXFLAGS": "-target x86_64-linux-gnu.2.28",
            "CMAKE_PREFIX_PATH": CONFIG["cmakePrefixPath"],
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER": CONFIG["hostCc"],
            "PKG_CONFIG_PATH": CONFIG["pkgConfigPath"],
            "PKG_CONFIG_LIBDIR": CONFIG["pkgConfigPath"],
            "PKG_CONFIG_ALL_STATIC": "1",
            "PKG_CONFIG_ALLOW_CROSS": "1",
            "GDAL_HOME": CONFIG["gdal"],
            "GDAL_STATIC": "1",
            "MLN_STATIC_DEPS": "1",
            "MLN_PRECOMPILE": "0",
            "MLN_CMAKE_PYTHON_EXECUTABLE": os.path.realpath(sys.executable),
            "CARGO_TARGET_DIR": str(work / "target"),
            "TMPDIR": str(work / "tmp"),
            "CMAKE_GENERATOR": "Ninja",
            "CMAKE_TOOLCHAIN_FILE": str(work / "toolchain.cmake"),
            "CARGO_ENCODED_RUSTFLAGS": "\x1f".join([
                *(f"-Lnative={path}" for path in CONFIG["libraryPath"].split(":")),
                f"-Clinker={CONFIG['cc']}",
            ]),
        }
        for name, value in expected.items():
            with self.subTest(name=name):
                self.assertEqual(env[name], value)
        for name in ("CC", "CXX", "AR", "CXXSTDLIB"):
            for target in (TARGET, TARGET.replace("-", "_")):
                self.assertEqual(env[f"{name}_{target}"], expected[name])

    def test_isolates_native_nix_pkg_config_vcpkg_and_search_paths(self):
        removed = {
            "NIX_CFLAGS_COMPILE", "NIX_CFLAGS_LINK", "NIX_CFLAGS_COMPILE_FOR_TARGET",
            "NIX_LDFLAGS", "NIX_LDFLAGS_FOR_TARGET", "NIX_CC_WRAPPER_FLAGS_SET",
            "NIX_BINTOOLS_WRAPPER_FLAGS_SET", "PKG_CONFIG_SYSROOT_DIR",
            "PKG_CONFIG_PATH_x86_64_unknown_linux_gnu", "PKG_CONFIG_ALL_DYNAMIC",
            "VCPKG_ROOT", "VCPKG_INSTALLATION_ROOT", "VCPKG_DEFAULT_TRIPLET",
            "VCPKG_TARGET_TRIPLET", "LIBRARY_PATH", "C_INCLUDE_PATH",
            "CPLUS_INCLUDE_PATH", "CPPFLAGS", "LDFLAGS",
            "CMAKE_INCLUDE_PATH", "CMAKE_LIBRARY_PATH",
        }
        replaced = {
            "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "PKG_CONFIG_ALL_STATIC",
            "PKG_CONFIG_ALLOW_CROSS", "CMAKE_TOOLCHAIN_FILE", "GDAL_HOME",
            "GDAL_STATIC", "MLN_STATIC_DEPS", "MLN_PRECOMPILE", "CARGO_TARGET_DIR",
            "TMPDIR", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
            "CFLAGS", "CXXFLAGS", "CMAKE_PREFIX_PATH",
        }
        inherited = {name: "/native/should-not-leak" for name in removed | replaced}
        inherited.update({"HOME": "/home/map", "MLN_LOCAL_REPOSITORY": "/source/maplibre"})
        original = inherited.copy()
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        self.assertEqual(inherited, original)
        for name in removed:
            self.assertNotIn(name, env, name)
        for name in replaced:
            self.assertNotEqual(env[name], original[name], name)
        self.assertEqual(env["HOME"], inherited["HOME"])
        self.assertEqual(env["MLN_LOCAL_REPOSITORY"], inherited["MLN_LOCAL_REPOSITORY"])
        self.assertNotIn("/native/should-not-leak", env.values())

    def test_removes_target_pkg_config_overrides_without_mutating_caller(self):
        inherited = {
            "TARGET_PKG_CONFIG_PATH": "/native/pkgconfig",
            "TARGET_PKG_CONFIG_LIBDIR": "/native/lib/pkgconfig",
            "TARGET_PKG_CONFIG_SYSROOT_DIR": "/native/sysroot",
            "TARGET_PKG_CONFIG_ALL_STATIC": "0",
            "TARGET_PKG_CONFIG_ALL_DYNAMIC": "1",
            "TARGET_PKG_CONFIG_ALLOW_CROSS": "0",
        }
        original = inherited.copy()
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        self.assertEqual(inherited, original)
        for name in inherited:
            self.assertNotIn(name, env, name)
        self.assertFalse(any(name.startswith("TARGET_PKG_CONFIG_") for name in env))
        self.assertEqual(env["PKG_CONFIG_PATH"], CONFIG["pkgConfigPath"])
        self.assertEqual(env["PKG_CONFIG_LIBDIR"], CONFIG["pkgConfigPath"])
        self.assertEqual(env["PKG_CONFIG_ALL_STATIC"], "1")
        self.assertEqual(env["PKG_CONFIG_ALLOW_CROSS"], "1")

    def test_compiler_flags_pin_configured_glibc_baseline(self):
        for glibc in ("2.28", "2.31"):
            with self.subTest(glibc=glibc):
                config = {**CONFIG, "glibc": glibc}
                inherited = {"CFLAGS": "--target=x86_64-linux-gnu -I/native/include",
                             "CXXFLAGS": "--target=x86_64-linux-gnu -L/native/lib",
                             "CMAKE_PREFIX_PATH": "/native/prefix"}
                env = packaging.package_environment(config, inherited, ROOT)
                for name in ("CFLAGS", "CXXFLAGS"):
                    self.assertEqual(env[name], f"-target x86_64-linux-gnu.{glibc}")
                self.assertEqual(env["CMAKE_PREFIX_PATH"], config["cmakePrefixPath"])

    def test_missing_required_configuration_reports_field_before_environment_setup(self):
        for name in CONFIG:
            with self.subTest(name=name):
                config = CONFIG.copy()
                del config[name]
                with patch.object(packaging, "tool_environment") as tools:
                    with self.assertRaises(ValueError) as error:
                        packaging.package_environment(config, {}, ROOT)
                self.assertEqual(str(error.exception), "Missing Linux package configuration: " + name)
                tools.assert_not_called()

    def test_required_configuration_rejects_empty_or_non_string_values(self):
        for name in CONFIG:
            for value in ("", None, 0, [], {}):
                with self.subTest(name=name, value=value):
                    with self.assertRaises(ValueError) as error:
                        packaging.package_environment({**CONFIG, name: value}, {}, ROOT)
                    self.assertEqual(str(error.exception), "Missing Linux package configuration: " + name)

    def test_explicit_empty_rust_wrappers_override_cargo_user_configuration(self):
        for wrapper in (None, "sccache", "/tools/other-wrapper"):
            with self.subTest(wrapper=wrapper):
                inherited = {} if wrapper is None else {
                    "RUSTC_WRAPPER": wrapper, "RUSTC_WORKSPACE_WRAPPER": wrapper,
                }
                env = packaging.package_environment(CONFIG, inherited, ROOT)
                # Missing keys would let Cargo fall back to ~/.cargo/config.toml.
                self.assertEqual(env["RUSTC_WRAPPER"], "")
                self.assertEqual(env["RUSTC_WORKSPACE_WRAPPER"], "")

    def test_inherited_sccache_cannot_reach_compilers_or_launchers(self):
        inherited = {
            "PATH": "/tools/bin:/nix/store/hash-sccache/bin:/usr/bin",
            "SCCACHE_DIR": "/cache", "SCCACHE_SERVER_PORT": "4226",
            "RUSTC_WRAPPER": "/tools/sccache",
            "RUSTC_WORKSPACE_WRAPPER": "sccache",
            "CC": "sccache clang", "CXX": "sccache clang++", "AR": "sccache ar",
            "MLN_CMAKE_CXX_LAUNCHER": "/tools/sccache",
            "CMAKE_C_COMPILER_LAUNCHER": "sccache",
            "CMAKE_CXX_COMPILER_LAUNCHER": "/tools/sccache",
            "LD_PRELOAD": "/native/inject.so", "LD_LIBRARY_PATH": "/native/lib",
        }
        for name in ("CC", "CXX", "AR"):
            for target in (TARGET, TARGET.replace("-", "_")):
                inherited[f"{name}_{target}"] = "sccache native-tool"
        original = inherited.copy()
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        self.assertEqual(inherited, original)
        self.assertEqual(env["PATH"], "/tools/bin:/usr/bin")
        for name in ("CC", "CXX", "AR"):
            self.assertEqual(env[name], CONFIG[name.lower()])
            for target in (TARGET, TARGET.replace("-", "_")):
                self.assertEqual(env[f"{name}_{target}"], CONFIG[name.lower()])
        for name in ("MLN_CMAKE_CXX_LAUNCHER", "CMAKE_C_COMPILER_LAUNCHER",
                     "CMAKE_CXX_COMPILER_LAUNCHER"):
            self.assertEqual(env[name], "")
        for name in ("LD_PRELOAD", "LD_LIBRARY_PATH", "SCCACHE_DIR", "SCCACHE_SERVER_PORT"):
            self.assertNotIn(name, env)
        for name, value in env.items():
            self.assertNotIn("sccache", name.lower(), name)
            self.assertNotIn("sccache", value.lower(), name)

    def test_parses_rustflags_and_adds_only_nonempty_portable_library_paths(self):
        config = {**CONFIG, "libraryPath": ":/portable libraries/lib::/portable/ui/lib:"}
        inherited = {"RUSTFLAGS": '-C opt-level=2 --cfg \'feature="portable build"\''}
        env = packaging.package_environment(config, inherited, ROOT)
        self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f"), [
            "-C", "opt-level=2", "--cfg", 'feature="portable build"',
            "-Lnative=/portable libraries/lib", "-Lnative=/portable/ui/lib",
            f"-Clinker={CONFIG['cc']}",
        ])
        self.assertNotIn("RUSTFLAGS", env)
        self.assertIn("RUSTFLAGS", inherited)

    def test_encoded_rustflags_take_precedence_without_splitting_arguments(self):
        flags = ["--cfg", 'feature="portable build"', "-C", "debuginfo=1"]
        inherited = {"CARGO_ENCODED_RUSTFLAGS": "\x1f".join(flags), "RUSTFLAGS": "--ignored"}
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f"), flags + [
            *(f"-Lnative={path}" for path in CONFIG["libraryPath"].split(":")),
            f"-Clinker={CONFIG['cc']}",
        ])
        self.assertNotIn("RUSTFLAGS", env)

    def test_portable_linker_overrides_inherited_target_linker_flags(self):
        inherited = {"RUSTFLAGS": "-Clinker=/host/cc",
                     "LIBCLANG_PATH": "/nix/store/native-clang/lib"}
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        self.assertEqual(env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"], CONFIG["hostCc"])
        self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")[-1], f"-Clinker={CONFIG['cc']}")
        self.assertEqual(env["LIBCLANG_PATH"], inherited["LIBCLANG_PATH"])

    def test_rejects_unsupported_hosts_before_preparing_environment(self):
        for system, machine in (("Darwin", "x86_64"), ("Linux", "aarch64")):
            with self.subTest(system=system, machine=machine):
                with patch.object(packaging.platform, "system", return_value=system), \
                     patch.object(packaging.platform, "machine", return_value=machine), \
                     patch.object(packaging, "tool_environment") as tools:
                    with self.assertRaisesRegex(ValueError, "requires x86_64 Linux"):
                        packaging.package_environment(CONFIG, {}, ROOT)
                tools.assert_not_called()


class BuildInputsTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="map linux package ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def test_loads_devenv_output_configuration(self):
        config_path = self.root / "linux-package.json"
        config_path.write_text(json.dumps(CONFIG))
        result = subprocess.CompletedProcess(
            [], 0, stdout=json.dumps({"outputs.linux-package": str(config_path)}))
        env = {"PATH": "/tools/bin"}
        with patch.object(packaging.subprocess, "run", return_value=result) as run:
            self.assertEqual(packaging.load_config(self.root, env), CONFIG)
        run.assert_called_once_with(
            ["devenv", "build", "outputs.linux-package"], cwd=self.root, env=env,
            check=True, capture_output=False, stdout=subprocess.PIPE, text=True)

    def test_prepares_isolated_tmp_and_cross_cmake_toolchain(self):
        work = self.root / "dist/build/appimage"
        gdal = self.root / "portable-gdal"
        (gdal / "lib").mkdir(parents=True)
        (gdal / "lib/libgdal.a").write_bytes(b"!<arch>\n")
        packaging.prepare_build({
            "CARGO_TARGET_DIR": str(work / "target"), "TMPDIR": str(work / "tmp"),
            "GDAL_HOME": str(gdal),
        })
        self.assertTrue((work / "tmp").is_dir())
        self.assertEqual((work / "toolchain.cmake").read_text(),
                         "set(CMAKE_SYSTEM_NAME Linux)\n"
                         "set(CMAKE_SYSTEM_PROCESSOR x86_64)\n"
                         "set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)\n")

    def test_missing_portable_static_gdal_is_actionable(self):
        with self.assertRaisesRegex(ValueError, "Portable GDAL static archive is missing"):
            packaging.prepare_build({
                "CARGO_TARGET_DIR": str(self.root / "target"),
                "TMPDIR": str(self.root / "tmp"), "GDAL_HOME": str(self.root / "missing"),
            })


class MainTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(patch.object(packaging.platform, "system", return_value="Linux"))
        self.enterContext(patch.object(packaging.platform, "machine", return_value="x86_64"))
        self.enterContext(patch.object(packaging, "PROJECT_ROOT", ROOT))
        self.enterContext(patch.dict(os.environ, {"PATH": "/tools/bin"}, clear=True))
        self.load = self.enterContext(patch.object(packaging, "load_config", return_value=CONFIG))
        self.prepare = self.enterContext(patch.object(packaging, "prepare_build"))
        self.run = self.enterContext(patch.object(packaging.subprocess, "run"))
        self.bundle = self.enterContext(patch.object(packaging, "build_appimage"))

    def test_check_reports_prepared_environment_without_cargo_or_bundling(self):
        output = io.StringIO()
        with patch.object(sys, "argv", ["package_linux.py", "--check"]), \
             contextlib.redirect_stdout(output):
            packaging.main()
        self.load.assert_called_once_with(ROOT, packaging.tool_environment(os.environ))
        env = self.prepare.call_args.args[0]
        self.prepare.assert_called_once_with(env)
        self.assertEqual(env["CC"], CONFIG["cc"])
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["RUSTC_WORKSPACE_WRAPPER"], "")
        lines = output.getvalue().splitlines()
        for name in ("CC", "CXX", "GDAL_HOME", "CARGO_TARGET_DIR", "MLN_STATIC_DEPS",
                     "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "MLN_CMAKE_CXX_LAUNCHER"):
            self.assertIn(f"{name}={env[name]}", lines)
        self.run.assert_not_called()
        self.bundle.assert_not_called()

    def test_default_builds_requested_linux_target_before_bundling(self):
        with patch.object(sys, "argv", ["package_linux.py"]), \
             contextlib.redirect_stdout(io.StringIO()):
            packaging.main()
        env = self.prepare.call_args.args[0]
        self.run.assert_called_once_with(
            ["cargo", "build", "--release", "--locked", "--bin", "map", "--target", TARGET],
            cwd=ROOT, env=env, check=True)
        self.bundle.assert_called_once_with(
            Path(env["CARGO_TARGET_DIR"]) / TARGET / "release/map", CONFIG, ROOT, env)

    def test_errors_exit_nonzero_with_linux_packaging_context(self):
        errors = (
            ValueError("Portable GDAL static archive is missing"),
            RuntimeError("AppImage audit rejected nonportable dependency"),
            FileNotFoundError("missing packaging tool"),
            subprocess.CalledProcessError(2, ["cargo", "build"]),
        )
        stages = (self.load, self.prepare, self.run, self.bundle)
        for index, stage in enumerate(stages):
            for error in errors:
                with self.subTest(stage=index, error=error):
                    for mock in stages:
                        mock.reset_mock()
                    stage.side_effect = error
                    stderr = io.StringIO()
                    arguments = ["package_linux.py", "--check"] if index < 2 else ["package_linux.py"]
                    try:
                        with patch.object(sys, "argv", arguments), \
                             contextlib.redirect_stdout(io.StringIO()), \
                             contextlib.redirect_stderr(stderr), \
                             self.assertRaises(SystemExit) as exit:
                            packaging.main()
                    finally:
                        stage.side_effect = None
                    self.assertEqual(exit.exception.code, 1)
                    self.assertEqual(stderr.getvalue(), f"Linux packaging failed: {error}\n")
                    for later in stages[index + 1:]:
                        later.assert_not_called()

    def test_missing_config_reports_cli_context_without_building(self):
        for name in CONFIG:
            with self.subTest(name=name):
                config = CONFIG.copy()
                del config[name]
                self.load.return_value = config
                stderr = io.StringIO()
                with patch.object(sys, "argv", ["package_linux.py", "--check"]), \
                     contextlib.redirect_stderr(stderr), \
                     self.assertRaises(SystemExit) as exit:
                    packaging.main()
                self.assertEqual(exit.exception.code, 1)
                self.assertEqual(stderr.getvalue(),
                                 f"Linux packaging failed: Missing Linux package configuration: {name}\n")
                self.prepare.assert_not_called()
                self.run.assert_not_called()
                self.bundle.assert_not_called()

    def test_environment_errors_stop_check_before_build_preparation(self):
        with patch.object(packaging, "package_environment", side_effect=ValueError("unsupported host")), \
             patch.object(sys, "argv", ["package_linux.py", "--check"]), \
             contextlib.redirect_stderr(io.StringIO()) as stderr, \
             self.assertRaises(SystemExit) as exit:
            packaging.main()
        self.assertEqual(exit.exception.code, 1)
        self.assertIn("Linux packaging failed: unsupported host", stderr.getvalue())
        self.prepare.assert_not_called()
        self.run.assert_not_called()
        self.bundle.assert_not_called()


@unittest.skipUnless(os.environ.get("MAP_RUN_NATIVE_TESTS") == "1", "requires Nix native tools")
class NativeLinkerTests(unittest.TestCase):
    def test_libuv_metadata_resolves_static_archive_and_headers(self):
        config = packaging.load_config(packaging.PROJECT_ROOT, packaging.tool_environment(os.environ))
        env = packaging.package_environment(config, os.environ, packaging.PROJECT_ROOT)
        directories = {}
        for option, prefix in (("--libs-only-L", "-L"), ("--cflags-only-I", "-I")):
            result = subprocess.run(
                ["pkg-config", "--static", option, "libuv"], env=env,
                check=True, capture_output=True, text=True,
            )
            paths = [Path(flag.removeprefix(prefix)) for flag in shlex.split(result.stdout)
                     if flag.startswith(prefix)]
            self.assertTrue(paths, result.stdout)
            for path in paths:
                self.assertTrue(path.is_dir(), str(path))
            directories[prefix] = paths
        self.assertTrue(any((path / "libuv.a").is_file() for path in directories["-L"]))
        self.assertTrue(any((path / "uv.h").is_file() for path in directories["-I"]))

    def test_host_libcxx_links_and_loads_without_inherited_library_paths(self):
        compiler = os.environ.get("MAP_PACKAGE_HOST_LINKER")
        self.assertIsNotNone(compiler, "run native tests inside devenv shell")
        with tempfile.TemporaryDirectory(prefix="map host libcxx probe ") as temporary:
            root = Path(temporary)
            env = packaging.package_environment(CONFIG, dict(os.environ), root)
            env.pop("LD_LIBRARY_PATH", None)
            Path(env["TMPDIR"]).mkdir(parents=True)
            binary = root / "probe"
            result = subprocess.run(
                [compiler, "-x", "c", "-", "-o", str(binary),
                 "-Wl,--no-as-needed", "-lc++"],
                input="int main(void) { return 0; }\n", env=env,
                capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            dynamic = subprocess.run(
                ["readelf", "-d", str(binary)], env=env,
                check=True, capture_output=True, text=True,
            )
            self.assertIn("libc++.so", dynamic.stdout)
            subprocess.run([str(binary)], env=env, check=True, timeout=60)

    def test_same_triple_build_scripts_use_native_linker_and_target_uses_override(self):
        compiler = os.environ.get("MAP_PACKAGE_HOST_LINKER")
        self.assertIsNotNone(compiler, "run native tests inside devenv shell")
        version = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, text=True)
        host = next(line.removeprefix("host: ") for line in version.stdout.splitlines()
                    if line.startswith("host: "))
        with tempfile.TemporaryDirectory(prefix="map linker probe ") as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname = "linker-probe"\nversion = "0.0.0"\nedition = "2024"\n'
            )
            # C++ build dependencies (cxx/link-cplusplus) request libc++ even
            # though Cargo links the build script with the native C driver.
            (root / "build.rs").write_text(
                '#[link(name = "c++")]\nunsafe extern "C" {}\nfn main() {}\n'
            )
            (root / "src/main.rs").write_text("fn main() {}\n")
            # Reproduce x86_64 rustc's bundled-LLD preference even on ARM64.
            # The production host linker must override it, not execute this stub.
            bundled = root / "gcc-ld"
            bundled.mkdir()
            lld = bundled / "ld.lld"
            lld.write_text("#!/bin/sh\necho 'unexpected bundled LLD' >&2\nexit 99\n")
            lld.chmod(0o755)
            linkers = {}
            for name in ("native", "portable"):
                wrapper = root / name
                log = root / f"{name}.json"
                wrapper.write_text(
                    f"#!{os.path.realpath(sys.executable)}\n"
                    "import json, os, sys\n"
                    f"with open({str(log)!r}, 'w') as output:\n"
                    "    json.dump(sys.argv[1:], output)\n"
                    f"os.execv({compiler!r}, [{compiler!r}, '-B{bundled}', "
                    "'-fuse-ld=lld', *sys.argv[1:]])\n"
                )
                wrapper.chmod(0o755)
                linkers[name] = str(wrapper)
            config = {**CONFIG, "target": host, "hostCc": linkers["native"],
                      "cc": linkers["portable"]}
            inherited = dict(os.environ)
            for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
                inherited.pop(name, None)
            # Exercise same-triple Cargo behavior on any Linux developer machine;
            # the stand-in target linker forwards to the native linker, not Zig.
            with patch.object(packaging.platform, "machine", return_value="x86_64"):
                env = packaging.package_environment(config, inherited, root)
            Path(env["TMPDIR"]).mkdir(parents=True)
            result = subprocess.run(
                ["cargo", "build", "--release", "--offline", "--target", host],
                cwd=root, env=env, capture_output=True, text=True, timeout=120,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            native_args = json.loads((root / "native.json").read_text())
            target_args = json.loads((root / "portable.json").read_text())
            self.assertIn("-lc++", native_args)
            self.assertTrue(any("build_script_build" in arg for arg in native_args), native_args)
            self.assertFalse(any("build_script_build" in arg for arg in target_args), target_args)
            self.assertTrue(any("linker_probe" in arg for arg in target_args), target_args)
            build_script, = Path(env["CARGO_TARGET_DIR"]).glob("release/build/*/build-script-build")
            elf = subprocess.run(["readelf", "-l", str(build_script)], env=env,
                                 check=True, capture_output=True, text=True)
            self.assertRegex(elf.stdout, r"Requesting program interpreter: /nix/store/[^\s\]]+")


if __name__ == "__main__":
    unittest.main()
