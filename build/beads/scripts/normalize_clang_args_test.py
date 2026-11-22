# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import tempfile
import unittest

from build.bazel.scripts import build_utils
from build.beads.scripts import normalize_clang_args, path_normalizer


class MockPathNormalizer(path_normalizer.PathNormalizer):
    def normalize_path(self, path: str) -> str:
        if path == "../..":
            return "{SOURCE_ROOT}"
        if path.startswith("../../"):
            return f"{path[6:]}"
        return path


class NormalizeClangArgsTest(unittest.TestCase):
    def test_normalize_c_compile_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang "
            "-MD -MF obj/foo/bar.d "
            "-I../../src/include "
            "-I ../../other/include "
            "-DDEBUG=1 -D_GNU_SOURCE "
            "-fcolor-diagnostics "
            "-c ../../src/foo.c "
            "-o obj/src/foo.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_C_COMPILE
        )
        self.assertIn("clang", normalized)
        self.assertIn("-Isrc/include", normalized)
        self.assertIn("-Iother/include", normalized)
        self.assertIn("-DDEBUG=1", normalized)
        self.assertIn("-D_GNU_SOURCE", normalized)
        self.assertIn("src/foo.c", normalized)

        # Output/dep/diagnostic flags should be omitted
        self.assertNotIn("-MD", normalized)
        self.assertNotIn("-fcolor-diagnostics", normalized)
        self.assertNotIn("obj/src/foo.o", normalized)

    def test_normalize_cpp_compile_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang++ "
            "-std=c++20 "
            "-Wall -Wextra "
            "-O2 "
            "-isystem ../../prebuilt/include "
            "-c ../../src/main.cc "
            "-o obj/src/main.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_CPP_COMPILE
        )
        self.assertIn("clang", normalized)
        self.assertIn("-std=c++20", normalized)
        self.assertIn("-Wall", normalized)
        self.assertIn("-Wextra", normalized)
        self.assertIn("-O2", normalized)
        self.assertIn("-isystemprebuilt/include", normalized)
        self.assertIn("src/main.cc", normalized)

    def test_normalize_assemble_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang "
            "-D__ASSEMBLER__ "
            "-I../../src/arch/x64 "
            "-O2 -Wall -fstack-clash-protection "
            "-Wa,--mrelax-relocations=yes "
            "-c ../../src/arch/x64/entry.S "
            "-o obj/src/arch/x64/entry.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_ASSEMBLE
        )
        self.assertIn("clang", normalized)
        self.assertIn("-D__ASSEMBLER__", normalized)
        self.assertIn("-Isrc/arch/x64", normalized)
        self.assertIn("-Wa,--mrelax-relocations=yes", normalized)
        self.assertIn("src/arch/x64/entry.S", normalized)

        # C/C++-only flags should be omitted for assembly actions
        self.assertNotIn("-O2", normalized)
        self.assertNotIn("-Wall", normalized)
        self.assertNotIn("-fstack-clash-protection", normalized)

    def test_normalize_cpp_link_flags(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "../../prebuilt/third_party/clang/linux-x64/bin/clang++ "
            "--driver-mode=g++ "
            "-Wl,--Map=out/bin/my_bin.map "
            "-Wl,--color-diagnostics "
            "-Wl,-z,rel "
            "-Wl,--start-group "
            "-L../../prebuilt/lib "
            "-Wl,-L../../other/lib "
            "-lfoo -lbar "
            "obj/src/foo.o obj/src/bar.pic.o "
            "-o out/bin/my_bin"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_CPP_LINK
        )
        self.assertIn("clang", normalized)
        self.assertIn("-Lprebuilt/lib", normalized)
        self.assertIn("-Lother/lib", normalized)
        self.assertIn("-Wl,-zrel", normalized)

        # Object files, library inputs, and map files should be omitted
        self.assertNotIn("obj/src/foo.o", normalized)
        self.assertNotIn("obj/src/bar.pic.o", normalized)
        self.assertNotIn("--driver-mode=g++", normalized)
        self.assertNotIn("-Wl,--color-diagnostics", normalized)
        self.assertNotIn("-lfoo", normalized)
        self.assertNotIn("-lbar", normalized)
        self.assertNotIn("-Wl,--start-group", normalized)

    def test_normalize_prefix_map_flags_with_gn_normalizer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp) / "fuchsia"
            build_dir = fuchsia_dir / "out" / "default"
            fuchsia_dir.mkdir(parents=True)
            build_dir.mkdir(parents=True)

            gn_norm = path_normalizer.GnPathNormalizer(fuchsia_dir, build_dir)

            cmd_gn = (
                f"clang++ -ffile-prefix-map={fuchsia_dir}=../.. "
                f"-ffile-prefix-map={fuchsia_dir}/out=.. "
                f"-ffile-prefix-map={build_dir}=. "
                "-c ../../src/foo.cc -o obj/foo.o"
            )
            normalized_gn = normalize_clang_args.normalize_clang_cmd(
                cmd_gn,
                action_type=normalize_clang_args.ACTION_CPP_COMPILE,
                normalizer=gn_norm,
            )
            self.assertIn(
                "-ffile-prefix-map={SOURCE_ROOT}={SOURCE_ROOT}", normalized_gn
            )
            self.assertIn(
                "-ffile-prefix-map={OUT_ROOT}={OUT_ROOT}", normalized_gn
            )
            self.assertIn(
                "-ffile-prefix-map={BUILD_DIR}={BUILD_DIR}", normalized_gn
            )

            cmd_comp_dir = "clang++ -ffile-compilation-dir=. -c foo.cc -o foo.o"
            normalized_comp_dir = normalize_clang_args.normalize_clang_cmd(
                cmd_comp_dir,
                action_type=normalize_clang_args.ACTION_CPP_COMPILE,
                normalizer=gn_norm,
            )
            self.assertIn("-ffile-compilation-dir=.", normalized_comp_dir)

    def test_normalize_wl_two_arg_linker_flags(self) -> None:
        normalizer = MockPathNormalizer()

        norm_soname_comma = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-soname,libfoo.so -o out/libfoo.so",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_soname_eq = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,--soname=libfoo.so -o out/libfoo.so",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_soname_single_eq = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-soname=libfoo.so -o out/libfoo.so",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_soname_split = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-soname -Wl,libfoo.so -o out/libfoo.so",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        self.assertEqual(norm_soname_comma, norm_soname_eq)
        self.assertEqual(norm_soname_comma, norm_soname_single_eq)
        self.assertEqual(norm_soname_comma, norm_soname_split)
        self.assertIn("-Wl,-soname=libfoo.so", norm_soname_comma)

        norm_rpath_comma = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-rpath,/pkg/lib -Wl,-rpath,$ORIGIN/lib "
            "-Wl,-rpath,../../prebuilt/lib -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_rpath_eq = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,--rpath=/pkg/lib -Wl,-rpath=$ORIGIN/lib "
            "-Wl,--rpath=../../prebuilt/lib -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        self.assertEqual(norm_rpath_comma, norm_rpath_eq)
        self.assertIn("-Wl,-rpath=/pkg/lib", norm_rpath_comma)
        self.assertIn("-Wl,-rpath=$ORIGIN/lib", norm_rpath_comma)
        self.assertIn("-Wl,-rpath=prebuilt/lib", norm_rpath_comma)

        norm_mllvm_comma = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-mllvm,--stack-size-section -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_mllvm_eq = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,--mllvm=--stack-size-section -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        self.assertEqual(norm_mllvm_comma, norm_mllvm_eq)
        self.assertIn("-Wl,--mllvm=--stack-size-section", norm_mllvm_comma)

        norm_ignored = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-Map,out/bin/my_bin.map "
            "-Wl,-dependency-file,out/bin/my_bin.d -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        self.assertEqual(norm_ignored, ["clang"])

        norm_script_comma = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-T,../../src/link.ld "
            "-Wl,--version-script,../../src/syms.ld -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_script_eq = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,--script=../../src/link.ld "
            "-Wl,--version-script=../../src/syms.ld -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        norm_script_compact = normalize_clang_args.normalize_clang_cmd(
            "clang++ -Wl,-T../../src/link.ld "
            "-Wl,--version-script=../../src/syms.ld -o out/my_bin",
            normalizer,
            action_type=normalize_clang_args.ACTION_CPP_LINK,
        )
        self.assertEqual(norm_script_comma, norm_script_eq)
        self.assertEqual(norm_script_comma, norm_script_compact)
        self.assertIn("-Wl,-Tsrc/link.ld", norm_script_comma)
        self.assertIn("-Wl,--version-script=src/syms.ld", norm_script_comma)

    def test_normalize_root_include_dirs(self) -> None:
        normalizer = MockPathNormalizer()
        cmd = (
            "clang -I../.. -I../../gen -I../../src/my_lib "
            "-iquote../.. -iquote../../src/my_quote "
            "-c foo.c -o foo.o"
        )
        normalized = normalize_clang_args.normalize_clang_cmd(
            cmd, normalizer, action_type=normalize_clang_args.ACTION_C_COMPILE
        )
        self.assertNotIn("-I{SOURCE_ROOT}", normalized)
        self.assertNotIn("-Igen", normalized)
        self.assertNotIn("-iquote{SOURCE_ROOT}", normalized)
        self.assertIn("-Isrc/my_lib", normalized)
        self.assertIn("-iquotesrc/my_quote", normalized)

    def test_bazel_canonical_prefix_map_stripping(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp) / "fuchsia"
            build_dir = fuchsia_dir / "out" / "default"
            fuchsia_dir.mkdir(parents=True)
            build_dir.mkdir(parents=True)
            (fuchsia_dir / ".jiri_manifest").write_text("")
            build_utils.BazelPaths.write_topdir_config_for_test(
                fuchsia_dir, "gen/build/bazel"
            )

            bazel_norm = path_normalizer.BazelPathNormalizer(
                build_utils.BazelPaths(fuchsia_dir, build_dir)
            )
            gn_norm = path_normalizer.GnPathNormalizer(fuchsia_dir, build_dir)
            noop_norm = path_normalizer.MockPathNormalizer()

            # LINT.IfChange(in_tree_clang_prefix_map)
            pairs = (
                ("", "../../"),
                (".", "."),
                ("./", "../../"),
                ("./bazel-out/", "bazel-out/"),
                ("./external/", "external/"),
                ("./fuchsia_build_generated/", "fuchsia_build_generated/"),
                ("../", "../"),
                ("bazel-out/", "bazel-out/"),
                ("external/", "external/"),
                ("fuchsia_build_generated/", "fuchsia_build_generated/"),
                ("/", "/"),
            )
            # LINT.ThenChange(
            #   //build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/toolchain_utils.bzl:in_tree_clang_prefix_map,
            #   //build/beads/scripts/normalize_clang_args.py:in_tree_clang_prefix_map,
            #   //build/config/BUILD.bazel:in_tree_clang_prefix_map,
            # )
            self.assertSequenceEqual(
                normalize_clang_args._CANONICAL_BAZEL_CLANG_PREFIX_MAP_PAIRS,
                pairs,
            )
            dot_slash_idx = pairs.index(("./", "../../"))
            for guarded_dot_slash in (
                ("./bazel-out/", "bazel-out/"),
                ("./external/", "external/"),
                ("./fuchsia_build_generated/", "fuchsia_build_generated/"),
            ):
                self.assertGreater(
                    pairs.index(guarded_dot_slash), dot_slash_idx
                )
            canonical_flags = [
                f"{opt}={old}={new}"
                for opt in ("-fdebug-prefix-map", "-fcoverage-prefix-map")
                for old, new in pairs
            ] + [
                normalize_clang_args._CANONICAL_BAZEL_CLANG_MACRO_PREFIX_MAP_FLAG,
            ]
            cmd_tokens = (
                ["clang++", "-c", "src/foo.cc"]
                + canonical_flags
                + [
                    "-fdebug-prefix-map",
                    "=../../",
                    "-fcoverage-prefix-map",
                    "=../../",
                    "-fmacro-prefix-map",
                    "./=",
                ]
            )
            cmd = " ".join(cmd_tokens)
            for action in (
                normalize_clang_args.ACTION_C_COMPILE,
                normalize_clang_args.ACTION_CPP_COMPILE,
                normalize_clang_args.ACTION_ASSEMBLE,
            ):
                normalized = normalize_clang_args.normalize_clang_cmd(
                    cmd, bazel_norm, action_type=action
                )
                for flag in canonical_flags:
                    self.assertNotIn(flag, normalized)
                self.assertEqual(
                    normalized, ["clang", "{SOURCE_ROOT}/src/foo.cc"]
                )

            # Link actions retain canonical prefix-map flags to surface leaks.
            link_normalized = normalize_clang_args.normalize_clang_cmd(
                "clang++ -fdebug-prefix-map==../../ -fcoverage-prefix-map==../../ -fmacro-prefix-map=./= -o out/bin",
                bazel_norm,
                action_type=normalize_clang_args.ACTION_CPP_LINK,
            )
            self.assertIn("-fdebug-prefix-map==../../", link_normalized)
            self.assertIn("-fcoverage-prefix-map==../../", link_normalized)
            self.assertIn("-fmacro-prefix-map={SOURCE_ROOT}=", link_normalized)

            # Unexpected target-local overrides remain unstripped on compile actions.
            override_normalized = normalize_clang_args.normalize_clang_cmd(
                "clang++ -fdebug-prefix-map=/custom=/other -fcoverage-prefix-map=/custom=/other -fmacro-prefix-map=/custom=/other -c src/foo.cc",
                bazel_norm,
                action_type=normalize_clang_args.ACTION_CPP_COMPILE,
            )
            self.assertIn(
                "-fdebug-prefix-map=/custom=/other", override_normalized
            )
            self.assertIn(
                "-fcoverage-prefix-map=/custom=/other", override_normalized
            )
            self.assertIn(
                "-fmacro-prefix-map=/custom=/other", override_normalized
            )

            # GN normalizer resolves "../.."; noop normalizer keeps verbatim.
            self.assertEqual(
                normalize_clang_args.normalize_clang_arg(
                    "-fdebug-prefix-map==../../", gn_norm
                ),
                "-fdebug-prefix-map=={SOURCE_ROOT}/",
            )
            self.assertEqual(
                normalize_clang_args.normalize_clang_arg(
                    "-fcoverage-prefix-map==../../", gn_norm
                ),
                "-fcoverage-prefix-map=={SOURCE_ROOT}/",
            )
            self.assertEqual(
                normalize_clang_args.normalize_clang_arg(
                    "-fmacro-prefix-map=./=", gn_norm
                ),
                "-fmacro-prefix-map={BUILD_DIR}=",
            )
            self.assertEqual(
                normalize_clang_args.normalize_clang_arg(
                    "-fdebug-prefix-map==../../", noop_norm
                ),
                "-fdebug-prefix-map==../../",
            )
            self.assertEqual(
                normalize_clang_args.normalize_clang_arg(
                    "-fcoverage-prefix-map==../../", noop_norm
                ),
                "-fcoverage-prefix-map==../../",
            )
            self.assertEqual(
                normalize_clang_args.normalize_clang_arg(
                    "-fmacro-prefix-map=./=", noop_norm
                ),
                "-fmacro-prefix-map=./=",
            )


if __name__ == "__main__":
    unittest.main()
