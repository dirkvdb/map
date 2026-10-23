#!/usr/bin/env python3
"""macOS packaging regressions; native tests require MAP_RUN_NATIVE_TESTS=1.

Run with python3 -B -m unittest discover -s scripts/tests -v.
"""

import contextlib
import io
import os
from pathlib import Path
import platform
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import package_macos as packaging

ROOT = Path("/tmp/Map Project").resolve()

SDKROOT = "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
DEVELOPER_DIR = "/Applications/Xcode.app/Contents/Developer"
CLANG = "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang"
CLANGXX = CLANG + "++"
LD = "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/ld"
NATIVE_PATHS = (SDKROOT, DEVELOPER_DIR, CLANG, CLANGXX, LD)
LINK_FLAG = f"-Clink-arg=-fuse-ld={LD}"
RUST_VERSION = "rustc 1.90.0\nbinary: rustc\nhost: aarch64-apple-darwin\nrelease: 1.90.0\n"
ENVIRONMENT = {
    "MAP_PACKAGE_PKG_CONFIG_PATH": "/nix/store/png/lib/pkgconfig:/nix/store/gdal/lib/pkgconfig",
    "MAP_PACKAGE_GDAL_HOME": "/nix/store/gdal",
    "MAP_PACKAGE_LIBRARY_PATH": "/nix/store/png/lib:/nix/store/gdal/lib",
    "LIBRARY_PATH": "/old/lib",
    "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER": "/nix/store/clang/bin/clang",
    "CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER": "/old/x86-linker",
    "PKG_CONFIG_PATH": "/old/pkgconfig",
    "PKG_CONFIG_LIBDIR": "/old/libdir",
    "PKG_CONFIG_ALL_STATIC": "0",
    "GDAL_HOME": "/old/gdal",
    "GDAL_STATIC": "0",
    "MLN_STATIC_DEPS": "0",
    "CARGO_TARGET_DIR": "/old/mise/build",
    "MLN_LOCAL_REPOSITORY": "/old/maplibre",
    "MLN_DEPENDENCY_PREFIX": "/old/vcpkg",

    "DEVELOPER_DIR": "/nix/store/sdk",
    "SDKROOT": "/nix/store/sdk/MacOSX.sdk",
    "CC": "/nix/store/clang/bin/clang",
    "CXX": "/nix/store/clang/bin/clang++",
    "OBJC": "/old/clang",
    "OBJCXX": "/old/clang++",
    "LD": "/old/ld",
    "NIX_LDFLAGS": "-L/nix/store/static-overlay/lib -lc++",
    "NIX_CFLAGS_COMPILE": "-isystem /nix/store/include",
    "PATH": "/nix/store/tools/bin:/usr/bin",
}




def result(stdout="", returncode=0, stderr=""):
    return subprocess.CompletedProcess([], returncode, stdout=stdout, stderr=stderr)

class PackageEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.platform = patch.object(packaging.platform, "system", return_value="Darwin")
        self.platform.start()
        self.addCleanup(self.platform.stop)

    def test_curated_environment_and_isolated_native_lookups(self):
        original = ENVIRONMENT.copy()
        with patch.object(packaging.subprocess, "run", side_effect=[
            result(path + "\n") for path in (*NATIVE_PATHS, RUST_VERSION)
        ]) as run:
            env = packaging.package_environment(original, ROOT)
        self.assertEqual(original, ENVIRONMENT)
        expected = {
            "PKG_CONFIG_PATH": original["MAP_PACKAGE_PKG_CONFIG_PATH"],
            "PKG_CONFIG_LIBDIR": original["MAP_PACKAGE_PKG_CONFIG_PATH"],
            "PKG_CONFIG_ALL_STATIC": "1",
            "GDAL_HOME": original["MAP_PACKAGE_GDAL_HOME"],
            "GDAL_STATIC": "1",
            "MLN_STATIC_DEPS": "1",
            "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP": "none",
            "CARGO_TARGET_DIR": str(ROOT / "dist/build/devenv"),


            "MLN_CMAKE_PYTHON_EXECUTABLE": str(Path(packaging.sys.executable).resolve()),
            "SDKROOT": SDKROOT,
            "DEVELOPER_DIR": DEVELOPER_DIR,
            "CC": CLANG,
            "CXX": CLANGXX,

            "PATH": str(Path(LD).parent) + ":" + original["PATH"],

            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER": CLANG,

            "CARGO_ENCODED_RUSTFLAGS": "-Lnative=/nix/store/png/lib\x1f-Lnative=/nix/store/gdal/lib\x1f" + LINK_FLAG,
        }
        for name, value in expected.items():
            self.assertEqual(env[name], value, name)
        self.assertNotIn("MLN_CMAKE_SWIFT_COMPILER", env)
        removed = {"OBJC", "OBJCXX", "LD", "LIBRARY_PATH"}
        for name in removed:
            self.assertNotIn(name, env)
        for name in original.keys() - expected.keys() - removed:
            self.assertEqual(env[name], original[name], name)
        native_env = run.call_args_list[0].kwargs["env"]
        self.assertNotIn("DEVELOPER_DIR", native_env)
        self.assertNotIn("SDKROOT", native_env)
        self.assertEqual(native_env["CC"], original["CC"])
        self.assertEqual(native_env["NIX_LDFLAGS"], original["NIX_LDFLAGS"])
        self.assertEqual(run.call_args_list[:5], [
            call(command, env=native_env, cwd=ROOT, capture_output=True, text=True, check=False)
            for command in (
                ["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"],
                ["/usr/bin/xcode-select", "-p"],
                ["/usr/bin/xcrun", "--find", "clang"],
                ["/usr/bin/xcrun", "--find", "clang++"],
                ["/usr/bin/xcrun", "--find", "ld"],
            )
        ])
        self.assertEqual(run.call_count, 6)
        self.assertEqual(run.call_args_list[5].args[0], ["rustc", "-vV"])
        self.assertEqual(run.call_args_list[5].kwargs["env"]["CC"], CLANG)

    def test_pins_running_python_instead_of_native_path_or_stale_override(self):
        python = "/nix/store/python313/bin/python3"
        with patch.object(packaging.sys, "executable", python):
            env = self.configure({"MLN_CMAKE_PYTHON_EXECUTABLE": "/usr/bin/python3"})
        self.assertEqual(env["MLN_CMAKE_PYTHON_EXECUTABLE"], python)

    def test_rejects_python_older_than_webgpu_generator_requires(self):
        with patch.object(packaging.sys, "version_info", (3, 9, 6)):
            with patch.object(packaging.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "Python 3.10 or newer"):
                    packaging.package_environment(ENVIRONMENT, ROOT)
        run.assert_not_called()

    def test_disables_stripping_only_for_release_build_dependencies(self):
        env = self.configure({
            "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP": "symbols",
            "CARGO_PROFILE_RELEASE_STRIP": "symbols",
        })
        self.assertEqual(env["CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP"], "none")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_STRIP"], "symbols")

    def test_optional_stale_variables_need_not_exist(self):
        env = {name: ENVIRONMENT[name] for name in (
            "MAP_PACKAGE_PKG_CONFIG_PATH", "MAP_PACKAGE_GDAL_HOME", "MAP_PACKAGE_LIBRARY_PATH",
        )}
        with patch.object(packaging.subprocess, "run", side_effect=[
            result(path) for path in (*NATIVE_PATHS, RUST_VERSION)
        ]):
            configured = packaging.package_environment(env, Path("."))
        self.assertTrue(Path(configured["CARGO_TARGET_DIR"]).is_absolute())
        self.assertEqual(
            configured["CARGO_TARGET_DIR"], str(Path.cwd() / "dist/build/devenv"),
        )
        for name in ("MLN_LOCAL_REPOSITORY", "CFLAGS", "CXXFLAGS", "OBJC", "OBJCXX", "LD", "LIBRARY_PATH"):
            self.assertNotIn(name, configured)

    def test_preserves_inherited_maplibre_checkout(self):
        for repository in (str(ROOT / ".devenv/maplibre-native"), "/tmp/Alternate Checkout/maplibre-native", ""):
            with self.subTest(repository=repository):
                env = self.configure({"MLN_LOCAL_REPOSITORY": repository})
                self.assertEqual(env["MLN_LOCAL_REPOSITORY"], repository)

    def test_clears_inherited_compiler_and_library_search_overrides(self):
        overrides = {
            "OBJC": "/nix/store/clang/bin/clang",
            "OBJCXX": "/nix/store/clang/bin/clang++",
            "LD": "/nix/store/linker/bin/ld",
            "LIBRARY_PATH": "/unrelated/lib:/old/lib",
        }
        env = self.configure(overrides)
        for name in overrides:
            self.assertNotIn(name, env)
        self.assertEqual(env["CC"], CLANG)
        self.assertEqual(env["CXX"], CLANGXX)
        self.assertIn(LINK_FLAG, env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f"))

    def configure(self, overrides=None, version=RUST_VERSION, native_paths=NATIVE_PATHS):
        env = ENVIRONMENT.copy()
        env.update(overrides or {})
        with patch.object(packaging.subprocess, "run", side_effect=[
            result(path) for path in (*native_paths, version)
        ]):
            return packaging.package_environment(env, ROOT)

    def test_native_tool_directories_precede_nix_and_are_deduplicated(self):
        clang_dir = str(Path(CLANG).parent)
        ld = "/Applications/Native Linker/bin/ld"
        ld_dir = str(Path(ld).parent)
        inherited = f"/nix/store/tools/bin:{clang_dir}:/usr/bin:{ld_dir}:/nix/store/tools/bin"
        env = self.configure(
            {"PATH": inherited},
            native_paths=(SDKROOT, DEVELOPER_DIR, CLANG, CLANGXX, ld),
        )
        self.assertEqual(env["PATH"].split(":"), [ld_dir, clang_dir, "/nix/store/tools/bin", "/usr/bin"])
        self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")[-1], f"-Clink-arg=-fuse-ld={ld}")

    def test_shared_native_tool_directory_is_prepended_only_once(self):
        env = self.configure()
        entries = env["PATH"].split(":")
        self.assertEqual(entries[0], str(Path(LD).parent))
        self.assertEqual(entries.count(str(Path(LD).parent)), 1)
        self.assertEqual(entries[1:], ENVIRONMENT["PATH"].split(":"))

    def test_x86_host_linker_overrides_inherited_setting(self):
        env = self.configure(version=RUST_VERSION.replace("aarch64", "x86_64"))
        self.assertEqual(env["CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER"], CLANG)

    def test_rustflags_are_parsed_and_encoded_with_native_searches(self):
        env = self.configure({"RUSTFLAGS": '-C opt-level=2 --cfg \'feature="with spaces"\''})
        self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f"), [
            "-C", "opt-level=2", "--cfg", 'feature="with spaces"',
            "-Lnative=/nix/store/png/lib", "-Lnative=/nix/store/gdal/lib", LINK_FLAG,
        ])
        self.assertNotIn("RUSTFLAGS", env)

    def test_encoded_flags_take_precedence_and_preserve_argument_boundaries(self):
        for encoded in ("", "--cfg\x1ffeature=\"with spaces\""):
            with self.subTest(encoded=encoded):
                env = self.configure({"CARGO_ENCODED_RUSTFLAGS": encoded, "RUSTFLAGS": "'invalid"})
                prefix = encoded + "\x1f" if encoded else ""
                self.assertEqual(
                    env["CARGO_ENCODED_RUSTFLAGS"],
                    prefix + "-Lnative=/nix/store/png/lib\x1f-Lnative=/nix/store/gdal/lib\x1f" + LINK_FLAG,
                )
                self.assertNotIn("RUSTFLAGS", env)

    def test_library_paths_preserve_spaces_and_skip_empty_search_entries(self):
        paths = "/tmp/My Libraries/lib::/nix/store/gdal/lib:"
        env = self.configure({"MAP_PACKAGE_LIBRARY_PATH": paths})
        self.assertNotIn("LIBRARY_PATH", env)
        self.assertEqual(env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f"), [
            "-Lnative=/tmp/My Libraries/lib", "-Lnative=/nix/store/gdal/lib", LINK_FLAG,
        ])

    def test_c_flags_are_preserved_verbatim(self):
        for flags in (
            {"CFLAGS": '  -O2 -DNAME="two words"  ', "CXXFLAGS": "-std=c++20 -isysroot '/custom sdk'"},
            {"CFLAGS": "", "CXXFLAGS": ""},
        ):
            with self.subTest(flags=flags):
                env = self.configure(flags)
                for name, value in flags.items():
                    self.assertEqual(env[name], value, name)

    def test_invalid_rust_host_is_rejected(self):
        for version in ("", "host: aarch64-unknown-linux-gnu", "host: aarch64-apple-darwin\nhost: x86_64-apple-darwin"):
            with self.subTest(version=version):
                with self.assertRaisesRegex(ValueError, "unsupported or missing macOS host"):
                    self.configure(version=version)

    def test_rustc_failure_reports_diagnostics(self):
        with patch.object(packaging.subprocess, "run", side_effect=[
            *[result(path) for path in NATIVE_PATHS],
            result(returncode=1, stderr="rust toolchain unavailable"),
        ]):
            with self.assertRaisesRegex(RuntimeError, "rustc -vV.*exit 1.*rust toolchain unavailable"):
                packaging.package_environment(ENVIRONMENT, ROOT)

    def test_requires_darwin_before_running_commands(self):
        with patch.object(packaging.platform, "system", return_value="Linux"):
            with patch.object(packaging.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "requires Darwin"):
                    packaging.package_environment(ENVIRONMENT, ROOT)
        run.assert_not_called()

    def test_requires_nonempty_devenv_configuration(self):
        for name in ("MAP_PACKAGE_PKG_CONFIG_PATH", "MAP_PACKAGE_GDAL_HOME", "MAP_PACKAGE_LIBRARY_PATH"):
            for value in (None, "", " \t"):
                env = ENVIRONMENT.copy()
                if value is None:
                    del env[name]
                else:
                    env[name] = value
                with self.subTest(name=name, value=value):
                    with patch.object(packaging.subprocess, "run") as run:
                        with self.assertRaisesRegex(ValueError, name):
                            packaging.package_environment(env, ROOT)
                    run.assert_not_called()


    def test_invalid_native_paths_are_rejected(self):
        paths = NATIVE_PATHS
        names = ("SDKROOT", "DEVELOPER_DIR", "CC", "CXX", "LD")
        for index, name in enumerate(names):
            for output in ("", " \t", "relative/path", "/one/path\n/two/path"):
                with self.subTest(name=name, output=output):
                    results = [result(path) for path in paths[:index]] + [result(output)]
                    with patch.object(packaging.subprocess, "run", side_effect=results):
                        with self.assertRaisesRegex(ValueError, f"invalid {name} path"):
                            packaging.package_environment(ENVIRONMENT, ROOT)

    def test_native_lookup_failures_report_diagnostics(self):
        for index, command in ((0, "xcrun"), (1, "xcode-select"), (2, "xcrun"), (3, "xcrun"), (4, "xcrun")):
            with self.subTest(command=command):
                results = [result(path) for path in NATIVE_PATHS[:index]]
                results.append(result(returncode=1, stderr="native tools unavailable"))
                with patch.object(packaging.subprocess, "run", side_effect=results):
                    with self.assertRaisesRegex(RuntimeError, f"{command}.*exit 1.*native tools unavailable"):
                        packaging.package_environment(ENVIRONMENT, ROOT)

BINARY = "/tmp/My App.app/Contents/MacOS/My App"
SYSTEM_DYLIB = "/usr/lib/libSystem.B.dylib"
SYSTEM_FRAMEWORK = "/System/Library/Frameworks/AppKit.framework/Versions/C/AppKit"


def dependency_line(path):
    return f"\t{path} (compatibility version 1.0.0, current version 1351.0.0)\n"


def output_for(*paths):
    return BINARY + ":\n" + "".join(dependency_line(path) for path in paths)


class ParseDependenciesTests(unittest.TestCase):
    def test_strips_version_suffix_and_binary_header(self):
        self.assertEqual(
            packaging.parse_dependencies(output_for(SYSTEM_DYLIB, SYSTEM_FRAMEWORK), BINARY),
            [SYSTEM_DYLIB, SYSTEM_FRAMEWORK],
        )

    def test_preserves_spaces_and_parentheses_in_paths(self):
        paths = [
            "/System/Library/Frameworks/My Framework (Extra).framework/My Framework",
            "/opt/My Libraries/lib example.dylib",
        ]
        self.assertEqual(packaging.parse_dependencies(output_for(*paths), BINARY), paths)

    def test_universal_binary_headers(self):
        output = "".join(
            f"{BINARY} (architecture {architecture}):\n" + dependency_line(SYSTEM_DYLIB)
            for architecture in ("arm64", "x86_64")
        )
        self.assertEqual(packaging.parse_dependencies(output, BINARY), [SYSTEM_DYLIB] * 2)

    def test_malformed_output_fails_closed(self):
        malformed = [
            "",
            " \n\t\n",
            BINARY + ":\n",
            dependency_line(SYSTEM_DYLIB),
            "/wrong/binary:\n" + dependency_line(SYSTEM_DYLIB),
            output_for(SYSTEM_DYLIB) + "unexpected diagnostic\n",
            BINARY + ":\n\t/usr/lib/libSystem.B.dylib\n",
            BINARY + ":\n" + SYSTEM_DYLIB + " (compatibility version 1.0.0, current version 1.0.0)\n",
            BINARY + ":\n\t/usr/lib/libSystem.B.dylib (compatibility version bad, current version 1.0.0)\n",
            output_for(SYSTEM_DYLIB).rstrip() + " trailing garbage\n",
            BINARY + ":\n" + BINARY + ":\n" + dependency_line(SYSTEM_DYLIB),
            output_for(SYSTEM_DYLIB) + BINARY + " (architecture arm64):\n",
        ]
        for output in malformed:
            with self.subTest(output=output), self.assertRaises(ValueError):
                packaging.parse_dependencies(output, BINARY)

class CheckDependenciesTests(unittest.TestCase):
    def run_check(self, output, returncode=0, stderr=""):
        result = subprocess.CompletedProcess([], returncode, stdout=output, stderr=stderr)
        with patch.object(packaging.subprocess, "run", return_value=result) as run:
            packaging.check_dependencies(BINARY)
        run.assert_called_once_with(
            ["/usr/bin/otool", "-L", BINARY], capture_output=True, text=True, check=False,
        )

    def test_accepts_system_libraries_and_frameworks(self):
        self.run_check(output_for(
            SYSTEM_DYLIB, SYSTEM_FRAMEWORK,
            "/System/Library/Frameworks/My Framework.framework/My Framework",
        ))

    def test_rejects_non_system_paths(self):
        paths = [
            "@rpath/libfoo.dylib",
            "@loader_path/libfoo.dylib",
            "@executable_path/libfoo.dylib",
            "/opt/homebrew/opt/foo/lib/libfoo.dylib",
            "/usr/local/opt/foo/lib/libfoo.dylib",
            "/nix/store/abc-foo/lib/libfoo.dylib",
            "libfoo.dylib",
            "../lib/libfoo.dylib",
            "/opt/My Libraries/libfoo.dylib",
            "/usr/library/libfoo.dylib",
            "/System/LibraryExtra/libfoo.dylib",
        ]
        for path in paths:
            with self.subTest(path=path), self.assertRaises(ValueError) as error:
                self.run_check(output_for(SYSTEM_DYLIB, path))
            self.assertIn(path, str(error.exception))

    def test_lists_all_offending_dependencies(self):
        paths = ["@rpath/libfoo.dylib", "/opt/homebrew/lib/libbar.dylib"]
        with self.assertRaises(ValueError) as error:
            self.run_check(output_for(SYSTEM_DYLIB, *paths))
        self.assertEqual(str(error.exception), "Non-system dependencies:\n  " + "\n  ".join(paths))

    def test_malformed_output_is_not_accepted(self):
        with self.assertRaises(ValueError):
            self.run_check(output_for(SYSTEM_DYLIB) + "unrecognized output\n")

    def test_otool_failure_even_with_valid_stdout(self):
        with self.assertRaisesRegex(RuntimeError, "otool failed .*exit 1.*not an object file"):
            self.run_check(output_for(SYSTEM_DYLIB), returncode=1, stderr="not an object file\n")

    def test_otool_failure_without_diagnostics(self):
        with self.assertRaisesRegex(RuntimeError, "no diagnostic output"):
            self.run_check("", returncode=1)

    def test_missing_otool(self):
        with patch.object(packaging.subprocess, "run", side_effect=FileNotFoundError("otool")):
            with self.assertRaises(OSError):
                packaging.check_dependencies(BINARY)

class CheckBundleTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.bundle = Path(temporary.name) / "My App.app"
        self.contents = self.bundle / "Contents"
        self.executable = self.contents / "MacOS" / "My App"
        self.icon = self.contents / "Resources" / "My Icon.icns"
        self.info_path = self.contents / "Info.plist"
        self.executable.parent.mkdir(parents=True)
        self.icon.parent.mkdir()
        self.executable.write_bytes(b"executable")
        self.icon.write_bytes(b"icon")
        self.info = {"CFBundleExecutable": "My App", "CFBundleIconFile": "My Icon"}
        self.write_info()
        self.check = self.enterContext(patch.object(packaging, "check_dependencies"))

    def write_info(self, fmt=plistlib.FMT_XML):
        self.info_path.write_bytes(plistlib.dumps(self.info, fmt=fmt))

    def test_checks_bundle_executable_and_adds_icon_suffix(self):
        packaging.check_bundle(self.bundle)
        self.check.assert_called_once_with(str(self.executable))

    def test_accepts_explicit_icon_suffix_and_binary_plist(self):
        self.info["CFBundleIconFile"] = "My Icon.icns"
        self.write_info(fmt=plistlib.FMT_BINARY)
        packaging.check_bundle(str(self.bundle))
        self.check.assert_called_once_with(str(self.executable))

    def test_preserves_existing_icon_suffix(self):
        self.info["CFBundleIconFile"] = "My Icon.png"
        self.icon.rename(self.icon.with_suffix(".png"))
        self.write_info()
        packaging.check_bundle(self.bundle)
        self.check.assert_called_once_with(str(self.executable))

    def test_missing_plist(self):
        self.info_path.unlink()
        with self.assertRaises(FileNotFoundError) as error:
            packaging.check_bundle(self.bundle)
        self.assertIn(str(self.info_path), str(error.exception))
        self.check.assert_not_called()

    def test_invalid_plist(self):
        for data in (b"not a plist", b"<?xml version='1.0'?><plist><dict>", b"bplist00broken"):
            with self.subTest(data=data):
                self.info_path.write_bytes(data)
                with self.assertRaisesRegex(ValueError, "Invalid bundle Info.plist") as error:
                    packaging.check_bundle(self.bundle)
                self.assertIn(str(self.info_path), str(error.exception))
        self.check.assert_not_called()

    def test_plist_requires_dictionary(self):
        self.info = ["not", "a dictionary"]
        self.write_info()
        with self.assertRaisesRegex(ValueError, "must contain a dictionary"):
            packaging.check_bundle(self.bundle)
        self.check.assert_not_called()

    def test_missing_or_invalid_metadata(self):
        for key in ("CFBundleExecutable", "CFBundleIconFile"):
            for value in (None, "", "   ", 42, ["name"]):
                with self.subTest(key=key, value=value):
                    info = self.info.copy()
                    if value is None:
                        del info[key]
                    else:
                        info[key] = value
                    self.info_path.write_bytes(plistlib.dumps(info))
                    with self.assertRaisesRegex(ValueError, key):
                        packaging.check_bundle(self.bundle)
        self.check.assert_not_called()

    def test_missing_executable_or_icon(self):
        for path, label in ((self.executable, "executable"), (self.icon, "icon")):
            with self.subTest(path=path):
                data = path.read_bytes()
                path.unlink()
                with self.assertRaisesRegex(ValueError, f"Bundle {label} is missing") as error:
                    packaging.check_bundle(self.bundle)
                self.assertIn(str(path), str(error.exception))
                path.write_bytes(data)
        self.check.assert_not_called()

    def test_directories_are_not_executable_or_icon_files(self):
        for path, label in ((self.executable, "executable"), (self.icon, "icon")):
            with self.subTest(path=path):
                data = path.read_bytes()
                path.unlink()
                path.mkdir()
                with self.assertRaisesRegex(ValueError, f"Bundle {label} is missing or not a file"):
                    packaging.check_bundle(self.bundle)
                path.rmdir()
                path.write_bytes(data)
        self.check.assert_not_called()

    def test_empty_icon(self):
        self.icon.write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "Bundle icon is empty") as error:
            packaging.check_bundle(self.bundle)
        self.assertIn(str(self.icon), str(error.exception))
        self.check.assert_not_called()

    def test_dependency_failure_propagates(self):
        self.check.side_effect = ValueError("Non-system dependencies:\n  @rpath/libfoo.dylib")
        with self.assertRaisesRegex(ValueError, "Non-system dependencies"):
            packaging.check_bundle(self.bundle)
        self.check.assert_called_once_with(str(self.executable))



class CheckEnvironmentTests(unittest.TestCase):
    def test_reports_selected_environment_and_static_libraries(self):
        env = {
            **ENVIRONMENT,
            "PKG_CONFIG_LIBDIR": ENVIRONMENT["MAP_PACKAGE_PKG_CONFIG_PATH"],
            "SDKROOT": SDKROOT,
            "DEVELOPER_DIR": DEVELOPER_DIR,
            "CC": CLANG,
            "CXX": CLANGXX,
            "CARGO_ENCODED_RUSTFLAGS": LINK_FLAG,
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER": CLANG,
        }
        command = ["pkg-config", "--static", "--libs", *packaging.STATIC_PACKAGES]
        self.assertEqual(packaging.STATIC_PACKAGES, (
            "libpng", "libjpeg", "libwebp", "libcurl", "sqlite3", "zlib", "gdal",
        ))
        output = io.StringIO()
        with patch.object(packaging, "capture", return_value="-lpng -ljpeg -lwebp -lcurl -lsqlite3 -lz -lgdal") as capture:
            with contextlib.redirect_stdout(output):
                packaging.check_environment(env, ROOT)
        capture.assert_called_once_with(command, env, ROOT)
        lines = output.getvalue().splitlines()
        for name in (
            "PKG_CONFIG_PATH", "PKG_CONFIG_LIBDIR", "GDAL_HOME", "CARGO_TARGET_DIR",
            "MLN_LOCAL_REPOSITORY", "SDKROOT", "CC", "CXX",
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER",
        ):
            self.assertIn(f"{name}={env[name]}", lines)
        printed_names = {line.partition("=")[0] for line in lines}
        for name in ("OBJC", "OBJCXX", "CFLAGS", "CXXFLAGS", "LD", "LIBRARY_PATH", "MLN_DEPENDENCY_PREFIX"):
            self.assertNotIn(name, printed_names)
        self.assertIn(" ".join(command) + ":", lines)
        self.assertIn("-lgdal", output.getvalue())

    def test_check_reports_inherited_or_unset_maplibre_checkout(self):
        for repository in (None, "/tmp/Alternate Checkout/maplibre-native"):
            with self.subTest(repository=repository):
                env = ENVIRONMENT.copy()
                if repository is None:
                    env.pop("MLN_LOCAL_REPOSITORY")
                else:
                    env["MLN_LOCAL_REPOSITORY"] = repository
                output = io.StringIO()
                with patch.object(packaging.subprocess, "run", return_value=result("-lgdal")):
                    with contextlib.redirect_stdout(output):
                        packaging.check_environment(env, ROOT)
                expected = "<unset>" if repository is None else repository
                self.assertIn("MLN_LOCAL_REPOSITORY=" + expected, output.getvalue().splitlines())


    def test_pkgconfig_failure_reports_missing_dependency(self):
        with patch.object(packaging.subprocess, "run", return_value=result(
            returncode=1, stderr="Package libjpeg was not found",
        )):
            with contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "pkg-config.*exit 1.*libjpeg was not found"):
                    packaging.check_environment(ENVIRONMENT, ROOT)


    def test_capture_falls_back_to_stdout_or_no_diagnostics(self):
        for stdout, detail in (("failure on stdout", "failure on stdout"), ("", "no diagnostic output")):
            with self.subTest(stdout=stdout):
                with patch.object(packaging.subprocess, "run", return_value=result(stdout, 1)):
                    with self.assertRaisesRegex(RuntimeError, detail):
                        packaging.capture(["pkg-config"], ENVIRONMENT, ROOT)


class CaptureTests(unittest.TestCase):
    def test_success_returns_stripped_stdout_with_requested_environment_and_root(self):
        command = ["pkg-config", "--version"]
        with patch.object(packaging.subprocess, "run", return_value=result("  2.3.0\n")) as run:
            self.assertEqual(packaging.capture(command, ENVIRONMENT, ROOT), "2.3.0")
        run.assert_called_once_with(
            command, env=ENVIRONMENT, cwd=ROOT, capture_output=True, text=True, check=False,
        )

    def test_failure_prefers_stderr_and_includes_command_and_exit_status(self):
        with patch.object(packaging.subprocess, "run", return_value=result("stdout detail", 7, "stderr detail\n")):
            with self.assertRaisesRegex(RuntimeError, "pkg-config --libs failed .*exit 7.*stderr detail") as error:
                packaging.capture(["pkg-config", "--libs"], ENVIRONMENT, ROOT)
        self.assertNotIn("stdout detail", str(error.exception))


class BuildDmgTests(unittest.TestCase):
    def test_checks_artifacts_before_bundling_signing_and_disk_image_creation(self):
        with tempfile.TemporaryDirectory(prefix="map-package-") as temporary:
            root = Path(temporary)
            env = {"CARGO_TARGET_DIR": str(root / "build")}
            release = root / "build/release"
            events = []

            def run(command, **kwargs):
                events.append(("run", list(command)))
                self.assertEqual(kwargs["cwd"], root)
                self.assertTrue(kwargs["check"])
                expected = {**env, "CARGO_BUNDLE_SKIP_BUILD": "1"} if list(command[:2]) == ["cargo", "bundle"] else env
                self.assertEqual(kwargs["env"], expected)

            with patch.object(packaging.subprocess, "run", side_effect=run):
                with patch.object(packaging, "check_dependencies", side_effect=lambda binary: events.append(("binary", binary))) as binary_check:
                    with patch.object(packaging, "check_bundle", side_effect=lambda app: events.append(("app", app))) as app_check:
                        with contextlib.redirect_stdout(io.StringIO()):
                            packaging.build_dmg(env, root)

            binary_check.assert_called_once_with(str(release / "map"))
            app_check.assert_called_once_with(release / "bundle/osx/Map.app")
            build = next(index for index, (kind, value) in enumerate(events) if kind == "run" and value[:2] == ["cargo", "build"])
            bundle = next(index for index, (kind, value) in enumerate(events) if kind == "run" and value[:2] == ["cargo", "bundle"])
            self.assertIn("--locked", events[build][1])
            self.assertLess(build, events.index(("binary", str(release / "map"))))
            self.assertLess(events.index(("binary", str(release / "map"))), bundle)
            app_guard = events.index(("app", release / "bundle/osx/Map.app"))
            self.assertLess(bundle, app_guard)
            for tool in ("/usr/bin/codesign", "/usr/bin/hdiutil"):
                invocations = [index for index, (kind, value) in enumerate(events) if kind == "run" and value[0] == tool]
                self.assertTrue(invocations, tool)
                for index in invocations:
                    self.assertLess(app_guard, index)

    def test_guard_failures_stop_packaging(self):
        for guard in ("check_dependencies", "check_bundle"):
            with self.subTest(guard=guard), tempfile.TemporaryDirectory(prefix="map-package-") as temporary:
                root = Path(temporary)
                env = {"CARGO_TARGET_DIR": str(root / "build")}
                with patch.object(packaging.subprocess, "run") as run:
                    with patch.object(packaging, "check_dependencies") as binary_check:
                        with patch.object(packaging, "check_bundle") as app_check:
                            failed_check = binary_check if guard == "check_dependencies" else app_check
                            failed_check.side_effect = ValueError("artifact rejected")
                            with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, "artifact rejected"):
                                packaging.build_dmg(env, root)
                commands = [list(invocation.args[0]) for invocation in run.call_args_list]
                if guard == "check_dependencies":
                    self.assertFalse(any(command[:2] == ["cargo", "bundle"] for command in commands))
                    app_check.assert_not_called()
                self.assertFalse(any(command[0] in ("/usr/bin/codesign", "/usr/bin/hdiutil") for command in commands))


class MainTests(unittest.TestCase):
    def setUp(self):
        self.env = {"prepared": "environment"}
        self.prepare = self.enterContext(patch.object(packaging, "package_environment", return_value=self.env))
        self.build = self.enterContext(patch.object(packaging, "build_dmg"))
        self.check = self.enterContext(patch.object(packaging, "check_environment"))

    def test_default_builds_with_prepared_environment(self):
        with patch.object(sys, "argv", ["package_macos.py"]):
            packaging.main()
        self.prepare.assert_called_once_with(packaging.os.environ, packaging.PROJECT_ROOT)
        self.build.assert_called_once_with(self.env, packaging.PROJECT_ROOT)
        self.check.assert_not_called()

    def test_check_validates_environment_without_building(self):
        with patch.object(sys, "argv", ["package_macos.py", "--check"]):
            packaging.main()
        self.prepare.assert_called_once_with(packaging.os.environ, packaging.PROJECT_ROOT)
        self.check.assert_called_once_with(self.env, packaging.PROJECT_ROOT)
        self.build.assert_not_called()

    def test_errors_exit_nonzero_with_packaging_context(self):
        errors = (
            ValueError("Non-system dependencies:\n  @rpath/libfoo.dylib"),
            RuntimeError("xcrun failed (exit 1): macOS SDK unavailable"),
            FileNotFoundError("cargo"),
            subprocess.CalledProcessError(2, ["cargo", "build"]),
        )
        for stage in (self.prepare, self.build, self.check):
            for error in errors:
                arguments = ["package_macos.py", "--check"] if stage is self.check else ["package_macos.py"]
                with self.subTest(stage=stage, error=error):
                    for mock in (self.prepare, self.build, self.check):
                        mock.reset_mock()
                    stage.side_effect = error
                    stderr = io.StringIO()
                    try:
                        with patch.object(sys, "argv", arguments):
                            with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as exit:
                                packaging.main()
                    finally:
                        stage.side_effect = None
                    self.assertEqual(exit.exception.code, 1)
                    self.assertIn("macOS packaging failed: " + str(error), stderr.getvalue())
                    if stage is self.prepare:
                        self.build.assert_not_called()
                        self.check.assert_not_called()

    def test_rejects_obsolete_targets_and_unknown_arguments_before_preparing(self):
        for arguments in ([BINARY], ["--bundle", "/tmp/My App.app"], ["--unknown"], ["--check", "extra"]):
            with self.subTest(arguments=arguments), patch.object(sys, "argv", ["package_macos.py", *arguments]):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit:
                    packaging.main()
                self.assertEqual(exit.exception.code, 2)
        self.prepare.assert_not_called()
        self.build.assert_not_called()
        self.check.assert_not_called()

    def test_help_exits_without_preparing_or_building(self):
        output = io.StringIO()
        with patch.object(sys, "argv", ["package_macos.py", "--help"]):
            with contextlib.redirect_stdout(output), self.assertRaises(SystemExit) as exit:
                packaging.main()
        self.assertEqual(exit.exception.code, 0)
        self.assertIn("--check", output.getvalue())
        self.prepare.assert_not_called()
        self.build.assert_not_called()
        self.check.assert_not_called()


@unittest.skipUnless(
    platform.system() == "Darwin" and os.environ.get("MAP_RUN_NATIVE_TESTS") == "1",
    "requires macOS, Rust, Apple command-line tools, and MAP_RUN_NATIVE_TESTS=1",
)
class NativeProcMacroTests(unittest.TestCase):
    def test_release_proc_macro_loads_in_packaging_environment(self):
        with tempfile.TemporaryDirectory(prefix="map-proc-macro-") as temporary:
            root = Path(temporary)
            macro = root / "macro"
            macro.mkdir()
            libraries = root / "lib"
            libraries.mkdir()
            pkgconfig = libraries / "pkgconfig"
            pkgconfig.mkdir()
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = ["macro"]\n\n'
                '[package]\nname = "macro-client"\nversion = "0.0.0"\nedition = "2021"\n\n'
                '[dependencies]\nprobe_macro = { path = "macro" }\n\n'
                '[[bin]]\nname = "macro-client"\npath = "main.rs"\n'
            )
            (root / "main.rs").write_text(
                'fn main() { assert_eq!(probe_macro::answer!(), 42); }\n'
            )
            (macro / "Cargo.toml").write_text(
                '[package]\nname = "probe_macro"\nversion = "0.0.0"\nedition = "2021"\n\n'
                '[lib]\nproc-macro = true\npath = "lib.rs"\n'
            )
            (macro / "lib.rs").write_text(
                'extern crate proc_macro;\nuse proc_macro::TokenStream;\n'
                '#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n'
                '    "42".parse().unwrap()\n}\n'
            )
            # This fixture has no native dependencies; no Nix realization or downloads are needed.
            inherited = dict(os.environ)
            inherited.update({
                "MAP_PACKAGE_PKG_CONFIG_PATH": str(pkgconfig),
                "MAP_PACKAGE_GDAL_HOME": str(root),
                "MAP_PACKAGE_LIBRARY_PATH": str(libraries),
                "OBJC": "/nonexistent/clang",
                "OBJCXX": "/nonexistent/clang++",
                "LD": "/nonexistent/ld",
                "LIBRARY_PATH": str(root / "stale-libraries"),
            })
            env = packaging.package_environment(inherited, root)
            for name in ("OBJC", "OBJCXX", "LD", "LIBRARY_PATH"):
                self.assertFalse(name in env, f"{name} must be unset")
            result = subprocess.run(
                ["cargo", "build", "--release", "--offline", "--manifest-path", str(root / "Cargo.toml")],
                cwd=root, env=env, capture_output=True, text=True, timeout=60, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            subprocess.run(
                [str(Path(env["CARGO_TARGET_DIR"]) / "release/macro-client")],
                cwd=root, env=env, timeout=10, check=True,
            )


if __name__ == "__main__":
    unittest.main()
