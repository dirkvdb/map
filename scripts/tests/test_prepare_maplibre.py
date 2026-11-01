"""Offline preparation, checkout safety and native build command regressions."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import prepare_maplibre as native


class CheckoutTests(unittest.TestCase):
    def setUp(self):
        self.temporary = self.enterContext(tempfile.TemporaryDirectory())
        self.repo = Path(self.temporary) / "ffi"
        self.repo.mkdir()
        self.git("init", "--quiet")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "Test")
        (self.repo / "tracked.txt").write_text("original\n")
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "initial")
        self.initial = self.git("rev-parse", "HEAD")

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True).strip()

    def test_clean_checkout_advances_to_pin(self):
        (self.repo / "tracked.txt").write_text("new\n")
        self.git("commit", "--quiet", "-am", "next")
        revision = self.git("rev-parse", "HEAD")
        self.git("checkout", "--quiet", "--detach", self.initial)
        with patch.object(native, "REVISION", revision):
            self.assertIsNone(native.checkout(self.repo))
        self.assertEqual(self.git("rev-parse", "HEAD"), revision)

    def test_modified_tracked_file_is_not_discarded(self):
        (self.repo / "tracked.txt").write_text("user progress\n")
        with self.assertRaisesRegex(RuntimeError, "Unrecorded modifications"):
            native.checkout(self.repo)
        self.assertEqual((self.repo / "tracked.txt").read_text(), "user progress\n")
        self.assertEqual(self.git("rev-parse", "HEAD"), self.initial)

    def test_staged_changes_are_rejected(self):
        (self.repo / "tracked.txt").write_text("staged\n")
        self.git("add", ".")
        with self.assertRaises(RuntimeError):
            native.checkout(self.repo)
        self.assertIn("staged", self.git("show", ":tracked.txt"))

    def test_untracked_changes_are_rejected(self):
        (self.repo / "notes.txt").write_text("keep me")
        with self.assertRaises(RuntimeError):
            native.checkout(self.repo)
        self.assertEqual((self.repo / "notes.txt").read_text(), "keep me")

    def test_managed_state_detects_untracked_content_edits(self):
        (self.repo / "generated.txt").write_text("patch content")
        record = {"state": native.worktree_state(self.repo)}
        native.require_managed_or_clean(self.repo, record)
        (self.repo / "generated.txt").write_text("user content")
        with self.assertRaisesRegex(RuntimeError, "Local modifications"):
            native.require_managed_or_clean(self.repo, record)

    def test_nested_submodule_edits_are_rejected(self):
        child = self.repo.parent / "child"
        subprocess.run(["git", "clone", "--quiet", str(self.repo), str(child)], check=True)
        self.git("-c", "protocol.file.allow=always", "submodule", "add", "--quiet", str(child), "vendor/child")
        self.git("commit", "--quiet", "-am", "submodule")
        nested = self.repo / "vendor/child/tracked.txt"
        nested.write_text("nested user progress")
        with self.assertRaises(RuntimeError):
            native.checkout(self.repo)
        self.assertEqual(nested.read_text(), "nested user progress")

    def test_patch_is_idempotent_and_conflicts_do_not_modify_files(self):
        (self.repo / "tracked.txt").write_text("patched\n")
        patch_file = self.repo.parent / "core.patch"
        patch_file.write_text(self.git("diff") + "\n")
        self.git("restore", "tracked.txt")
        native.apply_patch(self.repo, patch_file)
        native.apply_patch(self.repo, patch_file)
        self.assertEqual((self.repo / "tracked.txt").read_text(), "patched\n")
        (self.repo / "tracked.txt").write_text("user edit\n")
        with self.assertRaises(subprocess.CalledProcessError):
            native.apply_patch(self.repo, patch_file)
        self.assertEqual((self.repo / "tracked.txt").read_text(), "user edit\n")


class PreparationTests(unittest.TestCase):
    def test_recorded_preparation_skips_destructive_upstream_sync(self):
        record = {"fingerprint": "same"}
        with patch.object(native, "checkout", return_value=record), \
             patch.object(native, "preparation_fingerprint", return_value="same"), \
             patch.object(native, "run") as run:
            native.prepare(Path("/unused"), [])
        run.assert_not_called()

    def test_changed_patches_do_not_reset_managed_checkout(self):
        with patch.object(native, "checkout", return_value={"fingerprint": "old"}), \
             patch.object(native, "preparation_fingerprint", return_value="new"), \
             patch.object(native, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "patches changed"):
                native.prepare(Path("/unused"), [])
        run.assert_not_called()

    def test_first_preparation_uses_official_sync_then_core_patch(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "ffi"
            destination.mkdir()
            core_patch = Path(temporary) / "core.patch"
            with patch.object(native, "checkout", return_value=None), \
                 patch.object(native, "preparation_fingerprint", return_value="fingerprint"), \
                 patch.object(native, "worktree_state", return_value={"snapshot": "state"}), \
                 patch.object(native, "run") as run, patch.object(native, "apply_patch") as apply:
                native.prepare(destination, [core_patch])
            run.assert_called_once_with(["bash", "tools/deps/sync-submodules"], destination)
            apply.assert_called_once_with(destination / "third_party/maplibre-native", core_patch)
            self.assertEqual(native.read_record(destination)["state"], {"snapshot": "state"})


class BuildTests(unittest.TestCase):
    def test_host_backend_selection(self):
        hosts = [("Linux", "x86_64", "linux-gnu-x64-vulkan"),
                 ("Darwin", "arm64", "macos-arm64-metal"),
                 ("Windows", "AMD64", "windows-x64-vulkan")]
        for system, machine, preset in hosts:
            with self.subTest(system=system), \
                 patch.object(native.platform, "system", return_value=system), \
                 patch.object(native.platform, "machine", return_value=machine):
                self.assertEqual(native.TARGETS[native.host_target()], preset)

    def test_unsupported_host_is_explicit(self):
        with patch.object(native.platform, "system", return_value="FreeBSD"):
            with self.assertRaisesRegex(RuntimeError, "Unsupported native host"):
                native.host_target()

    def test_hooks_are_composed_without_expanding_cmake_paths(self):
        with tempfile.TemporaryDirectory(prefix="Map Project ") as temporary:
            root = Path(temporary)
            hooks = [root / "custom.cmake", root / "compositor ${literal}.cmake"]
            for hook in hooks:
                hook.touch()
            injection = native.write_injection(root / "build", hooks)
            for hook in hooks:
                self.assertIn(f"include([==[{hook.as_posix()}]==])", injection.read_text())

    def test_generated_hook_replaces_both_cached_and_normal_nix_linker(self):
        with tempfile.TemporaryDirectory() as temporary:
            injection = native.write_injection(Path(temporary), [])
            text = injection.read_text()
            self.assertIn('if(EXISTS "${map_real_linker}")', text)
            self.assertIn('set(CMAKE_LINKER "${map_real_linker}" CACHE FILEPATH', text)
            self.assertIn('set(CMAKE_LINKER "${map_real_linker}")', text)

    def test_linux_archive_finalization_restores_only_compositor_api(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "CMakeCache.txt").write_text("CMAKE_OBJCOPY:FILEPATH=/tools/llvm-objcopy\n")
            symbols = {"mln_custom_layer_add", "mln_custom_layer_vulkan_context", *native.COMPOSITOR_SYMBOLS}
            with patch.object(native, "run") as run, \
                 patch.object(native, "archive_symbols", return_value=symbols):
                native.finalize_extensions(root, "x86_64-unknown-linux-gnu")
            command = run.call_args.args[0]
            self.assertEqual(command[0], "/tools/llvm-objcopy")
            self.assertEqual(command[1:-1], [f"--globalize-symbol={name}" for name in native.COMPOSITOR_SYMBOLS])
            self.assertTrue(command[-1].endswith("complete-static/libmaplibre-native-c.a"))

    def test_archive_finalization_rejects_missing_extension_symbols(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "CMakeCache.txt").write_text("CMAKE_OBJCOPY:FILEPATH=/tools/llvm-objcopy\n")
            with patch.object(native, "run"), patch.object(native, "archive_symbols", return_value=set()):
                with self.assertRaisesRegex(RuntimeError, "lacks extension entry points"):
                    native.finalize_extensions(root, "x86_64-unknown-linux-gnu")

    def test_non_linux_finalization_does_not_use_elf_tools(self):
        with patch.object(native, "run") as run:
            native.finalize_extensions(Path("/absent"), "aarch64-apple-darwin")
        run.assert_not_called()

    def test_missing_compositor_hook_is_a_clear_error(self):
        with self.assertRaisesRegex(RuntimeError, "Missing extension injection hook"):
            native.write_injection(Path("/unused"), [Path("/absent/compositor/inject.cmake")])

    def test_official_preset_build_and_install_commands(self):
        destination = Path("/source")
        build_dir = Path("/build")
        install_dir = build_dir / "install"
        with patch.object(native, "write_injection", return_value=build_dir / "extensions.cmake"), \
             patch.object(native, "run") as run, patch.object(native, "validate_install") as validate, \
                          patch.object(native, "finalize_extensions") as finalize, \
             patch("builtins.print") as output:
            native.build(destination, build_dir, install_dir, "x86_64-unknown-linux-gnu", [], 2,
                         ["-DCMAKE_CXX_COMPILER_LAUNCHER=sccache"])
        configure, compile_call, install = [call.args[0] for call in run.call_args_list]
        self.assertEqual(configure[:3], ["cmake", "--preset", "linux-gnu-x64-vulkan"])
        self.assertIn("-DCMAKE_INSTALL_PREFIX=/build/install", configure)
        self.assertIn("-DBUILD_TESTING=OFF", configure)
        self.assertEqual(configure[-1], "-DCMAKE_PROJECT_INCLUDE=/build/extensions.cmake")
        self.assertEqual(compile_call, ["cmake", "--build", "/build", "--config", "Release", "--parallel", "2"])
        self.assertEqual(install, ["cmake", "--install", "/build", "--config", "Release"])
        finalize.assert_called_once_with(build_dir, "x86_64-unknown-linux-gnu")
        validate.assert_called_once()
        self.assertIn("MAPLIBRE_NATIVE_FFI_LOCAL_BUILD=/build/install", output.call_args.args[0])

    def test_msvc_requires_static_archive_not_dll_import_library(self):
        with tempfile.TemporaryDirectory() as temporary:
            install = Path(temporary)
            descriptor = install / native.DESCRIPTOR
            descriptor.parent.mkdir(parents=True)
            descriptor.write_text(json.dumps({
                "commit": native.REVISION,
                "platform": {"rustTarget": "x86_64-pc-windows-msvc"},
                "backend": "vulkan",
            }))
            (install / "lib").mkdir()
            (install / "lib/maplibre-native-c.lib").touch()
            with self.assertRaisesRegex(RuntimeError, "Missing complete static"):
                native.validate_install(install, "x86_64-pc-windows-msvc", "windows-x64-vulkan")
            (install / "lib/maplibre-native-c-static.lib").touch()
            native.validate_install(install, "x86_64-pc-windows-msvc", "windows-x64-vulkan")

    def test_build_failure_never_announces_an_install(self):
        with patch.object(native, "write_injection", return_value=Path("/hook")), \
             patch.object(native, "run", side_effect=subprocess.CalledProcessError(1, "cmake")), \
             patch("builtins.print") as output:
            with self.assertRaises(subprocess.CalledProcessError):
                native.build(Path("/source"), Path("/build"), Path("/install"),
                             "x86_64-unknown-linux-gnu", [], 1, [])
        output.assert_not_called()

    def test_install_descriptor_and_static_archive_are_required(self):
        with tempfile.TemporaryDirectory() as temporary:
            install = Path(temporary)
            descriptor = install / native.DESCRIPTOR
            descriptor.parent.mkdir(parents=True)
            data = {"commit": native.REVISION, "platform": {"rustTarget": "x86_64-unknown-linux-gnu"},
                    "backend": "vulkan"}
            descriptor.write_text(json.dumps(data))
            with self.assertRaisesRegex(RuntimeError, "Missing complete static"):
                native.validate_install(install, "x86_64-unknown-linux-gnu", "linux-gnu-x64-vulkan")
            (install / "lib").mkdir()
            (install / "lib/libmaplibre-native-c.a").touch()
            native.validate_install(install, "x86_64-unknown-linux-gnu", "linux-gnu-x64-vulkan")
            data["backend"] = "webgpu"
            descriptor.write_text(json.dumps(data))
            with self.assertRaisesRegex(RuntimeError, "artifact mismatch"):
                native.validate_install(install, "x86_64-unknown-linux-gnu", "linux-gnu-x64-vulkan")


if __name__ == "__main__":
    unittest.main()
