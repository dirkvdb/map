"""Hermetic tests of the actual patched core constructor, not a GPU integration test.

Only native-core files touched by the patch are staged below this crate's target/.
Minimal camera/paint test doubles avoid the unprepared core's unrelated dependencies.
No official FFI binding implementation is copied or built here.
"""

from pathlib import Path
import re
import subprocess
import unittest


CRATE = Path(__file__).resolve().parents[2]
CORE = CRATE.parents[1] / ".devenv/maplibre-native-ffi/third_party/maplibre-native"
PATCH = CRATE / "patches/0001-custom-layer-raster-parameters.patch"
HEADER = "include/mln/style/layers/custom_layer_render_parameters.hpp"
CONSTRUCTOR = "src/mln/style/layers/custom_layer_render_parameters.cpp"


def run(arguments, directory):
    result = subprocess.run(arguments, cwd=directory, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"{arguments}:\n{result.stdout}\n{result.stderr}")
    return result


class CorePatchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.stage = CRATE / "target/core-patch-tests"
        cls.stage.mkdir(parents=True, exist_ok=True)
        names = re.findall(r"^--- a/(.*)$", PATCH.read_text(), re.MULTILINE)
        for name in names:
            original = run(["git", "--no-pager", "show", f"HEAD:{name}"], CORE).stdout
            destination = cls.stage / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(original)
        original_header = cls.stage / "unpatched" / HEADER
        original_header.parent.mkdir(parents=True, exist_ok=True)
        original_header.write_text((cls.stage / HEADER).read_text())
        run(["git", "init", "--quiet"], cls.stage)
        # These official patches touch the same core files. Prepare only those
        # files to prove this extension applies after official FFI preparation.
        official = CORE.parents[1] / "patches/maplibre-native"
        includes = [f"--include={name}" for name in names]
        for name in ("0019-legacy-annotations-option.patch", "0055-shared-glyph-cache.patch"):
            run(["git", "apply", *includes, str(official / name)], cls.stage)
        run(["git", "apply", "--check", str(PATCH)], cls.stage)
        run(["git", "apply", str(PATCH)], cls.stage)

    def test_original_header_is_rejected_by_compile_guard(self):
        result = subprocess.run(
            ["clang++", "-std=c++20", "-fsyntax-only", "-Wno-unused-command-line-argument",
             f"-I{self.stage / 'unpatched/include'}", "-x", "c++", "native/core_patch_required.hpp"],
            cwd=CRATE, capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires patches/0001-custom-layer-raster-parameters.patch", result.stderr)

    def test_cmake_compilation_requires_the_core_patch(self):
        for patched in (False, True):
            with self.subTest(patched=patched):
                build = self.stage / f"cmake-guard-{patched}"
                headers = self.stage / ("include" if patched else "unpatched/include")
                run(["cmake", "-S", "tests/cmake", "-B", str(build),
                     f"-DCMAKE_PROJECT_INCLUDE={CRATE / 'cmake/inject.cmake'}",
                     f"-DMLN_CUSTOM_TEST_CORE_INCLUDE={headers}"], CRATE)
                result = subprocess.run(
                    ["cmake", "--build", str(build), "--target", "core_patch_contract"],
                    cwd=CRATE, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode == 0, patched, result.stderr)
                if not patched:
                    self.assertIn("requires patches/0001-custom-layer-raster-parameters.patch",
                                  result.stdout + result.stderr)

    def test_real_patched_constructor_preserves_registration_and_depth_order(self):
        for backend in ("VULKAN", "METAL"):
            with self.subTest(backend=backend):
                binary = self.stage / f"raster-parameters-{backend.lower()}"
                result = run(
                    ["clang++", "-std=c++20", "-Werror", "-Wall", "-Wextra",
                     "-Wno-unused-command-line-argument", "-DMLN_RENDER_BACKEND_OPENGL=0",
                     f"-DMLN_RENDER_BACKEND_{backend}=1", "-Inative",
                     "-Itests/native/fixtures", f"-I{self.stage / 'include'}",
                     str(self.stage / CONSTRUCTOR), "tests/native/raster_parameters.cpp",
                     "-o", str(binary)], CRATE,
                )
                self.assertEqual(result.returncode, 0)
                self.assertIn("all layer positions", run([str(binary)], CRATE).stdout)

    def test_pre_render_runs_only_after_groups_are_finalized(self):
        custom = (self.stage / "src/mln/renderer/layers/render_custom_layer.cpp").read_text()
        update = custom.split("void RenderCustomLayer::update", 1)[1]
        self.assertNotIn("callPreRender(", update)
        self.assertIn("layerGroup->addLayerTweaker(layerTweaker)", update)
        self.assertIn("callPreRender(host, parameters.context, parameters)", custom)
        renderer = (self.stage / "src/mln/renderer/renderer_impl.cpp").read_text()
        finalize = renderer.index("orchestrator.processChanges();")
        count = renderer.index("parameters.layerGroupCount =")
        upload = renderer.index("parameters.layerGroupUpload = true;")
        end_upload = renderer.index("parameters.layerGroupUpload = false;")
        self.assertLess(finalize, count)
        self.assertLess(count, upload)
        self.assertLess(upload, end_upload)


if __name__ == "__main__":
    unittest.main()
