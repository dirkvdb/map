"""Offline Windows distribution regressions."""

import json
import os
from pathlib import Path
import shlex
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import package_windows as packaging

CONFIG = {
    "target": packaging.TARGET,
    "cc": "/mingw/bin/gcc", "cxx": "/mingw/bin/g++",
    "ar": "/mingw/bin/ar", "rc": "/mingw/bin/windres",
    "objdump": "/mingw/bin/objdump", "magick": "/tools/bin/magick",
    "toolPath": "/rust/bin:/mingw/bin",
    "pkgConfigPath": "/mingw/gdal/lib/pkgconfig:/mingw/libuv/lib/pkgconfig",
    "libraryPath": "/mingw/gdal/lib:/mingw/curl/lib",
    "cmakePrefixPath": "/mingw/gdal:/mingw/libuv",
    "bindgenArgs": "--target=x86_64-w64-windows-gnu -isystem /mingw/include",
    "cflags": "-DCURL_STATICLIB -isystem/mingw/mcfgthread/include",
    "gdal": "/mingw/gdal", "gpuiWindows": "/patched/gpui-windows",
    "gpuiPreWindows": "/patched/gpui-pre-windows",
}
ROOT = Path("/tmp/Map Project")


class PackageEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(patch.object(packaging.platform, "system", return_value="Linux"))

    def test_cross_compilers_do_not_replace_host_compilers(self):
        inherited = {"CC": "/host/clang", "CXX": "/host/clang++", "AR": "/host/ar"}
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        for name, config_key in (("CC", "cc"), ("CXX", "cxx"), ("AR", "ar")):
            self.assertEqual(env[name], inherited[name])
            for target in (packaging.TARGET, packaging.TARGET_KEY):
                self.assertEqual(env[f"{name}_{target}"], CONFIG[config_key])
        self.assertEqual(env["CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER"], CONFIG["cc"])
        self.assertEqual(env["CARGO_TARGET_DIR"], str(ROOT / "dist/build/windows/target"))

    def test_native_flags_and_dependency_overrides_cannot_leak(self):
        removed = ("NIX_CFLAGS_COMPILE", "NIX_LDFLAGS", "LIBRARY_PATH", "CFLAGS",
                   "BINDGEN_EXTRA_CLANG_ARGS", "VCPKG_ROOT", "CMAKE_TOOLCHAIN_FILE",
                   "PKG_CONFIG_SYSROOT_DIR", "TARGET_PKG_CONFIG_LIBDIR", "CARGO_BUILD_TARGET")
        inherited = dict.fromkeys(removed, "/host/should-not-leak")
        inherited.update({"HOME": "/home/map", "MLN_LOCAL_REPOSITORY": "/source/maplibre-native"})
        original = inherited.copy()
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        self.assertEqual(inherited, original)
        for name in removed:
            self.assertNotIn(name, env)
        self.assertEqual(env[f"PKG_CONFIG_LIBDIR_{packaging.TARGET_KEY}"], CONFIG["pkgConfigPath"])
        self.assertEqual(env["PKG_CONFIG_ALL_STATIC"], "1")
        self.assertEqual(env["GDAL_STATIC"], "1")
        self.assertEqual(env["MLN_STATIC_DEPS"], "1")
        self.assertEqual(env["MLN_LOCAL_REPOSITORY"], inherited["MLN_LOCAL_REPOSITORY"])
        self.assertEqual(env[f"BINDGEN_EXTRA_CLANG_ARGS_{packaging.TARGET_KEY}"], CONFIG["bindgenArgs"])

    def test_host_pkg_config_is_separate_from_windows_dependencies(self):
        env = packaging.package_environment(CONFIG, {
            "PKG_CONFIG_PATH": "/host/openssl/lib/pkgconfig",
            "PKG_CONFIG_LIBDIR": "/host/lib/pkgconfig",
        }, ROOT)
        self.assertEqual(env["PKG_CONFIG_PATH"], "/host/openssl/lib/pkgconfig")
        self.assertEqual(env["PKG_CONFIG_LIBDIR"], "/host/lib/pkgconfig")
        for target in (packaging.TARGET, packaging.TARGET_KEY):
            self.assertEqual(env[f"PKG_CONFIG_PATH_{target}"], CONFIG["pkgConfigPath"])
            self.assertEqual(env[f"PKG_CONFIG_LIBDIR_{target}"], CONFIG["pkgConfigPath"])

    def test_hyphenated_inherited_target_compilers_are_overridden(self):
        inherited = {f"{name}_{packaging.TARGET}": "/host/compiler"
                     for name in ("CC", "CXX", "AR", "RC", "CXXSTDLIB", "CFLAGS", "CXXFLAGS")}
        env = packaging.package_environment(CONFIG, inherited, ROOT)
        for name in inherited:
            self.assertNotEqual(env[name], inherited[name])

    def test_wrappers_disabled_and_static_link_flags_selected(self):
        env = packaging.package_environment(CONFIG, {
            "RUSTC_WRAPPER": "sccache", "MLN_CMAKE_CXX_LAUNCHER": "sccache",
            "PATH": "/tools/sccache/bin:/usr/bin", "RUSTFLAGS": "-Cdebuginfo=1",
        }, ROOT)
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["RUSTC_WORKSPACE_WRAPPER"], "")
        self.assertEqual(env["MLN_CMAKE_CXX_LAUNCHER"], "")
        self.assertNotIn("sccache", env["PATH"])
        flags = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        self.assertIn("-Cdebuginfo=1", flags)
        self.assertIn("-Ctarget-feature=+crt-static", flags)
        self.assertIn("-Clink-arg=-static", flags)
        self.assertIn("-Lnative=/mingw/gdal/lib", flags)
        self.assertNotIn("RUSTFLAGS", env)

    def test_configuration_is_validated(self):
        for name in CONFIG:
            with self.subTest(name=name):
                config = CONFIG.copy()
                del config[name]
                with self.assertRaisesRegex(ValueError, "Missing Windows package configuration: " + name):
                    packaging.package_environment(config, {}, ROOT)
        with self.assertRaisesRegex(ValueError, "Unsupported Windows target"):
            packaging.package_environment({**CONFIG, "target": "other"}, {}, ROOT)

    def test_non_linux_hosts_fail_before_loading_tools(self):
        with patch.object(packaging.platform, "system", return_value="Darwin"):
            with self.assertRaisesRegex(ValueError, "requires Linux"):
                packaging.package_environment(CONFIG, {}, ROOT)


class BuildTests(unittest.TestCase):
    def test_cargo_uses_locked_windows_release_and_scoped_gpui_override(self):
        resource = ROOT / "dist/build/windows/map-icon.o"
        command = packaging.build_command(CONFIG, resource)
        self.assertIn("--locked", command)
        self.assertIn("--release", command)
        self.assertIn("rustc", command)
        target_index = command.index("--target")
        self.assertEqual(command[target_index:target_index + 3], ["--target", packaging.TARGET, "--"])
        self.assertIn(f"link-arg={resource}", command)
        self.assertEqual(command[command.index("--") + 1:command.index("--") + 3], ["-l", "static:+whole-archive=stdc++"])
        self.assertLess(command.index("link-arg=-lmcfgthread"), command.index("link-arg=-lkernel32"))
        self.assertLess(command.index("link-arg=-lkernel32"), command.index("link-arg=-lntdll"))
        self.assertIn("link-arg=-lz", command)
        self.assertNotIn(str(resource), packaging.package_environment(CONFIG, {}, ROOT)["CARGO_ENCODED_RUSTFLAGS"])
        self.assertIn("paths=" + json.dumps([CONFIG["gpuiWindows"], CONFIG["gpuiPreWindows"]]), command)
        self.assertIn("profile.release.package.gpui-pre-windows.debug-assertions=true", command)
        self.assertIn("profile.release.package.gpui-fast-windows.debug-assertions=true", command)

    def test_toolchain_uses_windows_not_egl_or_gl(self):
        with tempfile.TemporaryDirectory(prefix="Map Project ") as directory:
            root = Path(directory)
            config = {**CONFIG, "gdal": str(root / "gdal")}
            (root / "gdal/lib").mkdir(parents=True)
            (root / "gdal/lib/libgdal.a").touch()
            for name in ("cc", "cxx", "ar", "rc", "objdump", "magick"):
                config[name] = str(root / name)
                Path(config[name]).touch()
            env = packaging.package_environment(config, {}, root)
            packaging.prepare_build(config, env)
            toolchain = (root / "dist/build/windows/toolchain.cmake").read_text()
            self.assertIn("set(CMAKE_SYSTEM_NAME Windows)", toolchain)
            self.assertIn('set(CMAKE_C_COMPILER "' + config["cc"] + '")', toolchain)
            self.assertIn("CMAKE_RC_COMPILER", toolchain)
            self.assertIn('set(ENV{PKG_CONFIG_LIBDIR} "' + CONFIG["pkgConfigPath"] + '")', toolchain)
            self.assertNotIn("EGL", toolchain)
            self.assertNotIn("OpenGL", toolchain)

    def test_only_valid_static_windows_binary_is_copied(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "map.exe"
            output = Path(directory) / "bundle/Map.exe"
            binary.write_bytes(b"PE fixture")
            result = subprocess.CompletedProcess([], 0, "map.exe: file format pei-x86-64\nSubsystem 00000002 (Windows GUI)\n DLL Name: KERNEL32.dll\n DLL Name: d3dcompiler_47.dll\n DLL Name: icuuc.dll\n010   Entry: ID: 0x000003, Value: 0x80000020\n018   Entry: ID: 0x00000e, Value: 0x80000040\n")
            for subsystem in ("00000002 (Windows GUI)", "00000003 (Windows CUI)"):
                with self.subTest(subsystem=subsystem):
                    report = result.stdout.replace("00000002 (Windows GUI)", subsystem)
                    with patch.object(packaging.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, report)):
                        self.assertEqual(packaging.package_executable(binary, output, CONFIG, {}), output)
                    self.assertEqual(output.read_bytes(), binary.read_bytes())


    def test_resources_are_generated_from_source_and_compiled_as_x86_64_coff(self):
        with tempfile.TemporaryDirectory(prefix="Map Project ") as directory:
            root = Path(directory)
            env = packaging.package_environment(CONFIG, {}, root)
            with patch.object(packaging.subprocess, "run") as run:
                resource = packaging.prepare_resources(CONFIG, env, root)
            icon_command, rc_command = [call.args[0] for call in run.call_args_list]
            self.assertEqual(icon_command[0], CONFIG["magick"])
            self.assertEqual(icon_command[1], str(root / "data/icon.png"))
            self.assertIn("icon:auto-resize=256,128,64,48,32,16", icon_command)
            self.assertEqual(rc_command[0], CONFIG["rc"])
            self.assertIn(str(root / "dist/windows.rc"), rc_command)
            self.assertIn("--target=pe-x86-64", rc_command)
            self.assertIn("--output-format=coff", rc_command)
            self.assertEqual(resource, root / "dist/build/windows/map-icon.o")
            for call in run.call_args_list:
                self.assertTrue(call.kwargs["check"])
                self.assertEqual(call.kwargs["env"], env)

    def test_resource_compilation_failure_stops_the_build(self):
        with tempfile.TemporaryDirectory() as directory:
            env = packaging.package_environment(CONFIG, {}, directory)
            with patch.object(packaging.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "magick")):
                with self.assertRaises(subprocess.CalledProcessError):
                    packaging.prepare_resources(CONFIG, env, directory)

    def test_missing_icon_resources_are_rejected_before_copying(self):
        for resources in ("", "010   Entry: ID: 0x000003, Value: 0x80000020\n",
                                  "010   Entry: ID: 0x00000e, Value: 0x80000020\n",
                          "030     Entry: ID: 0x000003, Value: 0x80000020\n038     Entry: ID: 0x00000e, Value: 0x80000040\n"):
            result = subprocess.CompletedProcess([], 0, "pei-x86-64\nSubsystem 00000002 (Windows GUI)\n" + resources)
            with patch.object(packaging.subprocess, "run", return_value=result):
                with patch.object(packaging.shutil, "copy2") as copy:
                    with self.assertRaisesRegex(ValueError, "missing embedded icon resources"):
                        packaging.package_executable(Path("map.exe"), Path("Map.exe"), CONFIG, {})
                    copy.assert_not_called()

    def test_non_windows_or_non_static_binary_is_rejected(self):
        for report in ("file format elf64-x86-64", "pei-x86-64\nSubsystem 00000002 (Windows GUI)\n DLL Name: libstdc++-6.dll\n010   Entry: ID: 0x000003, Value: 0x80000020\n018   Entry: ID: 0x00000e, Value: 0x80000040\n"):
            with self.subTest(report=report):
                result = subprocess.CompletedProcess([], 0, report)
                with patch.object(packaging.subprocess, "run", return_value=result):
                    with self.assertRaises(ValueError):
                        packaging.package_executable(Path("map.exe"), Path("Map.exe"), CONFIG, {})

    def test_external_icu_is_rejected_while_windows_system_icu_is_allowed(self):
        report = "pei-x86-64\nSubsystem 00000002 (Windows GUI)\n010   Entry: ID: 0x000003, Value: 0x80000020\n018   Entry: ID: 0x00000e, Value: 0x80000040\n DLL Name: icuuc76.dll\n"
        result = subprocess.CompletedProcess([], 0, report)
        with patch.object(packaging.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(ValueError, "Unexpected runtime DLL dependencies: icuuc76.dll"):
                packaging.package_executable(Path("map.exe"), Path("Map.exe"), CONFIG, {})

    def test_config_is_loaded_from_devenv_output(self):
        with tempfile.TemporaryDirectory() as directory:
            config_file = Path(directory) / "config.json"
            config_file.write_text(json.dumps(CONFIG))
            result = subprocess.CompletedProcess([], 0, json.dumps({"outputs.windows-package": str(config_file)}))
            with patch.object(packaging.subprocess, "run", return_value=result) as run:
                self.assertEqual(packaging.load_config(ROOT, {}), CONFIG)
            self.assertEqual(run.call_args.args[0], ["devenv", "build", "outputs.windows-package"])


@unittest.skipUnless(os.environ.get("MAP_RUN_NATIVE_TESTS") == "1", "requires Nix/MinGW native tools")
class NativeIconTests(unittest.TestCase):
    def test_proj_metadata_matches_static_mingw_archives(self):
        config = packaging.load_config(packaging.PROJECT_ROOT, packaging.tool_environment(os.environ))
        env = packaging.package_environment(config, os.environ, packaging.PROJECT_ROOT)
        env.update({"PKG_CONFIG_PATH": config["pkgConfigPath"],
                    "PKG_CONFIG_LIBDIR": config["pkgConfigPath"]})
        result = subprocess.run(["pkg-config", "--static", "--libs", "proj"],
                                env=env, check=True, capture_output=True, text=True)
        flags = shlex.split(result.stdout)
        self.assertIn("-lproj", flags)
        self.assertIn("-lstdc++", flags)
        self.assertNotIn("-lproj_9", flags)
        self.assertFalse(any(flag.startswith("-l/") for flag in flags), flags)
        self.assertTrue(any((Path(path) / "libpthread.a").is_file()
                            for path in config["libraryPath"].split(":")))

    def test_distribution_gpui_disables_optional_directx_debugging(self):
        config = packaging.load_config(packaging.PROJECT_ROOT, packaging.tool_environment(os.environ))
        for name in ("gpuiWindows", "gpuiPreWindows"):
            with self.subTest(crate=name):
                source = (Path(config[name]) / "src/directx_devices.rs").read_text()
                probe = source.split("fn check_debug_layer_available() -> bool {", 1)[1].split("}", 1)[0]
                self.assertIn("false", probe)
                self.assertNotIn("DXGIGetDebugInterface1", source)
                self.assertNotIn("Failed to get DXGI debug interface", source)

    def test_source_icon_is_embedded_in_a_real_windows_executable(self):
        config = packaging.load_config(packaging.PROJECT_ROOT, packaging.tool_environment(os.environ))
        with tempfile.TemporaryDirectory(prefix="Map icon smoke ") as directory:
            env = packaging.package_environment(config, os.environ, directory)
            resource = packaging.prepare_resources(config, env, packaging.PROJECT_ROOT)
            icon = resource.with_name("map.ico").read_bytes()
            self.assertEqual(struct.unpack_from("<HHH", icon), (0, 1, 6))
            dimensions = {(icon[6 + index * 16] or 256, icon[7 + index * 16] or 256)
                          for index in range(6)}
            self.assertEqual(dimensions, {(size, size) for size in (16, 32, 48, 64, 128, 256)})
            binary = Path(directory) / "icon-smoke.exe"
            subprocess.run([
                config["cc"], "-static", "-mwindows",
                *("-L" + path for path in config["libraryPath"].split(":") if path),
                "-x", "c", "-", "-x", "none", str(resource), "-o", str(binary),
            ], input="int main(void) { return 0; }\n", text=True, env=env, check=True)
            output = Path(directory) / "bundle/Map.exe"
            packaging.package_executable(binary, output, config, env)
            self.assertEqual(binary.read_bytes(), output.read_bytes())


if __name__ == "__main__":
    unittest.main()
