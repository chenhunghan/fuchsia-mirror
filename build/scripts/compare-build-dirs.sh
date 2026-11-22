#!/bin/bash
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

function usage() {
  cat <<EOF
usage: $0 [options] DIR1 DIR2 [RELPATH]

Compares two build directories for artifact differences.
Comparison logic tries to account for the file type in
choosing a diff strategy.

If RELPATH is provided, then compare DIR1/RELPATH vs. DIR2/RELPATH.
The joined path may reference a directory or file.

Run this from the Fuchsia source checkout root dir ($FUCHSIA_DIR),
because it references some tools from the source tree.

Example: Compare two clean builds with same output dir:
  fx set ...
  fx clean-build
  cp -p -r out/default out/default.bkp
  fx clean-build
  cp -p -r out/default out/default.bkp2
  $0 out/default.bkp out/default.bkp2

Valid options:
  --help     Print this message.
  --verbose  Increase verbosity: print debug messages, diffs for all
             files, and full file lists. Repeat for more debug output.
EOF
}

readonly script="$0"
# assume script is always with path prefix, e.g. "./$script"
readonly script_dir="${script%/*}"  # dirname
# readonly script_basename="${script##*/}"  # basename

# shellcheck source=/dev/null
source "$script_dir"/../../build/rbe/common-setup.sh

# shellcheck disable=SC2154  # default_project_root comes from common-setup.sh
project_root="$default_project_root"

readonly detail_diff="$project_root"/build/rbe/detail-diff.sh
readonly detail_diff_opts=(
  "-l=first"
  "-r=second"
)

DEBUG_LEVEL=0

# Debug messages go to fd 3 (a copy of the original stderr) so they don't get
# mixed into the captured diff output in diff_file_relpath().
exec 3>&2

# Print debug message if DEBUG_LEVEL is at least 1.
function debug() {
  [[ "${DEBUG_LEVEL}" -ge 1 ]] && echo >&3 "DEBUG: $*"
}

function debug2() {
  [[ "${DEBUG_LEVEL}" -ge 2 ]] && echo >&3 "DEBUG2: $*"
}

# json formatting (as pipe): jq . < stdin > stdout
readonly jq="$project_root"/prebuilt/third_party/jq/"$HOST_PLATFORM"/bin/jq

function json_format() {
  debug2 "json_format $1"
  "$jq" . < "$1" > "$1".formatted
}

# GLOBAL MUTABLES
# Accumulate paths to unexpected differences here.
# bash: array variables are not POSIX
unexpected_diffs=()
unexpected_matches=()
# These files match or differ, but we didn't know what to expect.
unclassified_diffs=()
unclassified_matches=()
# These files were skipped.
# It might be useful to compare these after ALL
# other differences have been eliminated.
skipped_files=()
# Ignored files are unimportant will never be compared.
ignored_files=()
unspecified_files=()
# These files exist in only one of the two dirs.
missing_files=()

# The diff_* functions below read `identical`, a local variable of the calling
# diff_file_relpath() that's visible through Bash's dynamic scoping. It's set
# when the prepass hashes show the two files are the same, so the functions
# can skip spawning a diff tool.

function diff_json() {
  [[ "$identical" == 1 ]] && return 0
  debug "diff_json $1 $2"
  # Most files are identical, so skip spawning jq twice plus diff.
  cmp -s "$1" "$2" && return 0
  json_format "$1" || { echo "Failed to format $1" ; return 1;}
  json_format "$2" || { echo "Failed to format $2" ; return 1;}
  diff -u "$1".formatted "$2".formatted
  # return with the exit code of diff
}

function diff_text() {
  [[ "$identical" == 1 ]] && return 0
  debug "diff_text $1 $2"
  diff -u "$1" "$2"
}

function diff_binary() {
  [[ "$identical" == 1 ]] && return 0
  # cmp is faster, as it terminates at first byte difference.
  debug "diff_binary $1 $2"
  cmp "$1" "$2" || "$detail_diff" "${detail_diff_opts[@]}" "$1" "$2"
}

function diff_zip() {
  [[ "$identical" == 1 ]] && return 0
  # cmp is faster, as it terminates at first byte difference.
  debug "diff_zip $1 $2"
  cmp "$1" "$2" || "$detail_diff" "${detail_diff_opts[@]}" "$1" "$2"
}

function exe_unstripped_expect() {
  # Print "diff/match/unknown" depending on whether this file is known to be
  # reproducible across identical builds.
  local path="$1"
  # e.g. exe.unstripped/tool_name
  base="$(basename "$path")"
  case "$base" in
    archivist* | \
    base_resolver | \
    cobalt | \
    cobalt_app_unittests | \
    codec_runner_sw_aac | \
    component_manager | \
    component_manager_boot_env | \
    component_manager_test)
    # TODO(fangism): add many more cases of known diffs
          echo "diff" ;;
    *) echo "match" ;;
  esac
}

function diff_file_relpath() {

  # $1 is left dir
  # $2 is right dir
  # $3 is a relative path under both dirs, and is itself not a directory.
  #   This file name is reported in diagnostics.
  # one could also use an all-inclusive diff tool like https://diffoscope.org/
  local -r left="$1/$3"
  local -r right="$2/$3"
  # The prepass keys its lookups by paths without a leading "./", so strip it
  # in case it came from a RELPATH argument.
  local -r common_path="${3#./}"
  local -r filebase="${common_path##*/}"  # basename
  local -r subdir="${common_path%/*}" # dirname

  # TODO(fangism): Some files are stored as blobs so content differences
  # appear as filename entry differences.  Skip these.  Perhaps silently?
  # Testing -r also allows for symlinks.
  test -r "$left" || {
    debug "$left does not exist"
    missing_files+=("$common_path")
    return
  }
  test -r "$right" || {
    debug "$right does not exist"
    missing_files+=("$common_path")
    return
  }

  # Most files match, so skip spawning a diff tool per file when the
  # precomputed hashes already show that the two files are identical.
  local identical=0
  local -r left_hash="${left_hashes["$common_path"]-}"
  [[ -n "$left_hash" && "$left_hash" == "${right_hashes["$common_path"]-}" ]] &&
    identical=1

  local expect=""

  # Classify each category of files with expectations in each case below:
  #   expect={diff,match,unknown,ignore,skip}; diff...
  # Leave blank for ignored files (not compared).
  # "unknown" means unclassified and could contain a mix of matches/diffs.
  # Goal:
  #   * Identify and classify known differences (eliminate unknowns).
  #   * Gradually reduce sources of differences.
  #
  # TODO(fangism): different expectations in clean-vs-clean /
  # clean-vs-incremental modes.
  # Most diffs are expected, so the output of this case statement is
  # captured (see the redirect on the closing `esac`) and only printed if the
  # result is unexpected, to make the unexpected diffs easy to find.
  case "$filebase" in
    # The exit status of this case statement will be used to determine
    # whether or not the given file is an erroneous diff.
    #
    # Generally:
    #   diff_text for text files that are expected to match
    #   diff_binary for binaries or known large textual differences

    # depfiles
    *.d) expect="match"; diff_text "$left" "$right" ;;

    # C++ object files (binary)
    # Nondeterminism due to __TIME__-stamping has been eliminated
    # and is continuously verified by another builder.
    *.o) expect="match"; diff_binary "$left" "$right" ;;
    *.so) expect="match"; diff_binary "$left" "$right" ;;
    *.dylib) expect="match"; diff_binary "$left" "$right" ;;

    *.a) expect="match"; diff_binary "$left" "$right" ;;

    # Rust libraries (binary)
    # There may be unexpected rmeta mismatches.
    # See http//https://fxbug.dev/42079524, https://github.com/rust-lang/rust/issues/113584
    *.rlib) expect="match"; diff_binary "$left" "$right" ;;
    *.rmeta) expect="match"; diff_binary "$left" "$right" ;;

    # Generated code
    *.rs)
      case "$common_path" in
        gen/src/*/qmi-protocol.rs)
          expect="diff" ;;  # ordering diff
        *) expect="match" ;;
      esac
      diff_text "$left" "$right"
      ;;

    memory_metrics_registry.cb.h)
      expect="diff"; diff_text "$left" "$right" ;;  # ordering diff

    # The following groups of files have known huge diffs,
    # so omit details from the general report, and diff_binary.
    meta.far) expect="match"; diff_binary "$left" "$right" ;;
    contents) expect=unknown; diff_binary "$left" "$right" ;;
    all_blobs.json) expect=skip ;;  # too big right now

    blobs.json) expect="diff"; diff_binary "$left" "$right" ;;
      # Many of blobs.json do actually match

    blob.manifest) expect="diff"; diff_binary "$left" "$right" ;;  # many hashes
    blobs.manifest) expect=unknown; diff_binary "$left" "$right" ;;
    package_manifest.json) expect="match"; diff_binary "$left" "$right" ;;
    targets.json)
      case "$common_path" in
        gen/gopaths/*) expect="match"; diff_json "$left" "$right"  ;;
        amber-files/repository/targets.json) expect=skip ;; # too big right now
        */recovery_repository/targets.json) expect=skip ;;
        */repository/targets.json) expect=skip ;;
        */keys/targets.json) expect="match"; diff_json "$left" "$right"  ;;
        *) expect="diff" ;;  # diffs: many hashes
      esac
      ;;
    *.targets.json) expect=skip ;;  # diffs: sig, expires

    outputs.json) expect=skip ;;  # ordering differences from zircon/tools/zither
    snapshot.json) expect=skip ;;  # diffs: sig, expires, version (use diff_json)
    *.root.json) expect=skip ;;  # diffs: sig, expires
    *.snapshot.json) expect=skip ;;  # diffs: sig, expires, version
    timestamp.json) expect="ignore" ;;  # diffs: sig, expires, version
    elf_sizes.json) expect="diff"; diff_json "$left" "$right" ;;  # diffs: build_id
    recovery-eng_blobs.json) expect="diff"; diff_json "$left" "$right" ;;  # diffs: bytes, merkle, size (ordering)
    *.zbi.json) expect=unknown; diff_json "$left" "$right" ;;  # diffs: crc32, size
    *.vbmeta) expect="skip" ;; # Always timestamped in the digital signature
    product_bundle.json)
      case "$common_path" in
        # Checked-in test data copied by //tools/make-fuchsia-vol's tests.
        */make-fuchsia-vol_test_data/product_bundle.json) expect="match"; diff_json "$left" "$right" ;;
        */tools/make-fuchsia-vol/test_data/product_bundle.json) expect="match"; diff_json "$left" "$right" ;;
        *) expect="diff"; diff_json "$left" "$right" ;;  # update package hash, due to vbmeta
      esac
      ;;
    update_packages.manifest.json) expect="diff"; diff_json "$left" "$right" ;;  # hashes
    ota_manifest) expect="diff"; diff_binary "$left" "$right" ;;  # vbmeta hash
    root.json)
      case "$common_path" in
        */repository/root.json) expect=skip ;; # diffs: sig, expires
        */recovery_repository/root.json) expect=skip ;; # diffs: sig, expires
        *) expect="match"; diff_json "$left" "$right"  ;;
      esac
      ;;
    # These files are known to have non-deterministic json field ordering due to a Bazel issue
    x64-emu-extra-large.json) expect=skip ;; # field ordering
    x64-emu-min.json) expect=skip ;; # field ordering
    x64-emu-recommended.json) expect=skip ;; # field ordering
    arm64-emu-extra-large.json) expect=skip ;; # field ordering
    arm64-emu-min.json) expect=skip ;; # field ordering
    arm64-emu-recommended.json) expect=skip ;; # field ordering

    images.json) expect=skip ;;  # too many diffs

    transfer.json) expect=skip ;;  # look like list of blob hashes
    *size_report.json | *size_report_product.json | *size_breakdown.txt | *size_budgets.json)
      expect=skip ;; # contains sizes of images which differ already and are expect=unknown.

    compile_commands.json) expect=skip ;;  # too many

    # printed types contain memory addresses
    test.stringarrays.api_summary.json ) expect="ignore" ;;
    test.experimentalzxctypes.api_summary.json) expect="ignore" ;;

    docs.goldens.json)
      # in fuchsia-idk-build-*/gen/zircon/vdso
      # looks like ordering differences
      expect="ignore" ;;

    # Generated Rust target summaries from cargo_toml_gen copy crate_root from
    # project.json, which for generated crates (such as FIDL Rust bindings)
    # embeds //out/<build_dir>/... paths.
    rust_targets.json | rustdoc_targets.json | rustdoc_host_targets.json) expect="ignore" ;;

    # Diff formatted JSON for readability.
    *.json)
      case "$common_path" in
        host-tools/goroot/src/cmd/interna/test2json/testdata/*.json)
          expect=unknown ;;
        *) expect="match" ;;
      esac
      diff_json "$left" "$right"
      ;;
    *.json.formatted) expect="ignore" ;;  # This is remant from an earlier diff.

    # recovery things
    recovery-eng_additional_boot_args.txt) expect="diff"; diff_text "$left" "$right" ;;  # hashes
    recovery-eng_pkgsvr_index) expect="diff"; diff_text "$left" "$right" ;;  # hashes

    update_packages.manifest) expect="diff"; diff_text "$left" "$right" ;;  # hashes

    *.image_assembly_inputs) expect=skip ;;  # too many blob/hash differences
    *.image_assembler_all_inputs) expect=skip ;;  # too many blob/hash differences

    manifest.fini) expect=skip ;;  # looks like ordering differences

    # Bazel-related outputs
    java.log)
      case "$common_path" in
        gen/build/bazel/*/java.log) expect="ignore" ;;  # log with timestamps
        *) expect=unknown ;;
      esac
      ;;

    lock)
      case "$common_path" in
        gen/build/bazel/*/lock) expect="ignore" ;;  # contains PID
        *) expect=unknown ;;
      esac
      ;;

    command_port)
      case "$common_path" in
        gen/build/bazel/*/command_port) expect="ignore" ;;  # randomly chosen port
        *) expect=unknown ;;
      esac
      ;;

    request_cookie)
      case "$common_path" in
        gen/build/bazel/*/request_cookie) expect="ignore" ;;  # looks like hash
        *) expect=unknown ;;
      esac
      ;;

    response_cookie)
      case "$common_path" in
        gen/build/bazel/*/response_cookie) expect="ignore" ;;  # looks like hash
        *) expect=unknown ;;
      esac
      ;;

    server_info.rawproto)
      case "$common_path" in
        gen/build/bazel/*/server_info.rawproto) expect="ignore" ;;  # probably PID
        *) expect=unknown ;;
      esac
      ;;

    server.starttime) expect="ignore" ;;  # timestamp
    server.pid.txt) expect="ignore" ;;  # contains PID

    command.log)
      case "$common_path" in
        gen/build/bazel/*/command.log) expect="ignore" ;;  # subject to build parallelism and completion ordering
        *) expect=unknown ;;
      esac
      ;;

    ffx.log)
      case "$common_path" in
        gen/build/bazel/*/cache/logs/ffx.log) expect="ignore" ;;  # log with timestamps
        *) expect=unknown ;;
      esac
      ;;
    uuid)  # uuids are unique and random
      case "$subdir" in
        *.ffx/metrics) expect="ignore" ;;
      esac
      ;;

    volatile-status.txt) expect="ignore" ;;  # bears timestamp

    test.cache_status)
      case "$common_path" in
        bazel-out/*/test.cache_status) expect="diff"; diff_binary "$left" "$right" ;;
        *) expect=unknown ;;
      esac
      ;;

    test.log)
      case "$common_path" in
        bazel-out/*/test.log) expect="ignore" ;;  # reports test time duration
        *) expect=unknown ;;
      esac
      ;;

    test.xml)
      case "$common_path" in
        bazel-out/*/test.xml) expect="ignore" ;;  # reports test time duration
        *) expect=unknown ;;
      esac
      ;;

    action_cache_v15.blaze) expect="ignore" ;;
    filename_index_v15.blaze) expect="ignore" ;;
    command.profile.gz) expect="ignore" ;;

    workspace-events.log*) expect="ignore" ;;

    # numbered bazel action logs (unordered)
    stdout-*) expect="ignore" ;;
    stderr-*) expect="ignore" ;;

    *.runfiles_manifest) expect="ignore" ;;  # absolute paths

    *.repo_mapping) expect="ignore" ;;

      # These list hashes of binaries that are already being
      # compared elsewhere, so this is redundant information.
    *.ids_txt) expect="ignore" ;;

    # Various binaries.
    *.blk) expect=unknown; diff_binary "$left" "$right" ;;
    *.vboot) expect=unknown; diff_binary "$left" "$right" ;;
    *.zbi) expect=unknown; diff_binary "$left" "$right" ;;

    # TODO(b/379173865): fidldoc.zip bears timestamps (nondeterministic)
    fidldoc.zip) expect="ignore" ;;

    # rustix build script compilation tests are attributed to absolute paths
    # in metadata, which causes false positives in bazel consistency checks
    rustix_test_can_compile) expect="ignore" ;;

    # Generated Cargo.toml files produced by cargo_toml_gen contain absolute
    # paths to the source tree, 3P rust_crates patches, and the output directory.
    Cargo.toml | Cargo_for_fuchsia_dir.toml) expect="ignore" ;;

    # python compiled files (nondeterministic)
    *.pyc) expect="ignore" ;;

    *.pyz | *.zip) expect="match"; diff_zip "$left" "$right" ;;
    # Most archives carry timestamp information of their contents.
    # One way to make this reproducible is to force a magic date/time
    # while archiving, which effectively removes time variance.
    # tarmaker copies file contents and mtimes from //prebuilt in the
    # checkout, so these match between two builds of the same checkout.
    qemu.tar.gz | femu.tar.gz) expect="match"; diff_binary "$left" "$right" ;;
    *.tar | *.tar.gz | *.tgz) expect="diff"; diff_binary "$left" "$right" ;;

    # Ignore ninja logs, as they bear timestamps,
    # and are non-essential build artifacts.
    .ninja.log) expect="ignore" ;;

    # Ignore filesystem access trace files (fsatrace logs).
    # They may contain nondeterministic paths to /proc/PID
    *_trace.txt) expect="ignore" ;;

    # like exe.unstripped/*.map files
    # .map files are side-effect outputs of linking binaries, and not consumed
    # anywhere else important.
    # Many of these (but not all) reference mktemp paths (Rust toolchain).
    *.map) expect="ignore" ;;

    # Ignore stamp files.
    *.stamp) expect="ignore" ;;

    # Ignore temporary and backup files.
    *.tmp) expect="ignore" ;;
    *.bak) expect="ignore" ;;
    *.bkp) expect="ignore" ;;

    # All others.
    # Binary files diffs will still only be reported tersely.
    # TODO(b/377967111): fix python hermetic test nondeterminism:
    rtc_conformance_test_bin) expect="diff"; diff_binary "$left" "$right" ;;
    *)
      # Most files that reach this case are text, so only spawn `file` if
      # the magic bytes indicate a binary that the case below could match.
      file_type=""
      magic=""
      LC_ALL=C read -r -N 4 magic < "$left"
      if [[ "$magic" == $'\x7fELF' || "$magic" == $'\xca\xfe\xba'* ]]
      then
        file_type="${left_types["$common_path"]-}"
        [[ -n "$file_type" ]] || file_type="$(file -b "$left")"
      fi
      case "$file_type" in
        # Binary examples:
        # ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV),... dynamically linked, interpreter ld.so.1, no section header
        # Mach-O universal binary with 2 architectures: [x86_64:Mach-O 64-bit executable x86_64] [arm64e:Mach-O 64-bit executable arm64e]
        *ELF*executable* | *Mach-O*binary*)
          case "$common_path" in
            exe.unstripped/*) expect="$(exe_unstripped_expect "$common_path")" ;;
            *) expect=unknown ;;
          esac
          diff_binary "$left" "$right"
          ;;
        # Assume non-binaries are text.
        *) expect="match"; diff_text "$left" "$right" ;;
      esac
  # Only printed below if the result is unexpected or --verbose was passed.
  esac > "$diff_output_file" 2>&1

  # Record unexpected and unclassified differences/matches.
  diff_status="$?"
  local show_diff_output=0
  [[ "$DEBUG_LEVEL" -ge 1 ]] && show_diff_output=1
  case "$expect" in
    match)
      if test "$diff_status" != 0
      then
        unexpected_diffs+=("$common_path")
        show_diff_output=1
      fi
      ;;
    diff)
      test "$diff_status" != 0 || unexpected_matches+=("$common_path") ;;
    unknown)
      if test "$diff_status" = 0
      then unclassified_matches+=("$common_path")
      else unclassified_diffs+=("$common_path")
      fi
      ;;
    ignore) ignored_files+=("$common_path") ;;
    skip) skipped_files+=("$common_path") ;;
    *) unspecified_files+=("$common_path") ;;
  esac

  if [[ "$show_diff_output" == 1 && -s "$diff_output_file" ]]
  then
    echo "======== DIFF: $common_path ========"
    cat "$diff_output_file"
    echo
  fi
}

function diff_select() {
  # $1 and $2 are two directories, e.g. "out/default"
  # $3 is the relative path down from $1 and $2, e.g. "subdir/" or "".
  # Avoid a $(printf) subshell per path unless verbose.
  [[ "${DEBUG_LEVEL}" -ge 2 ]] &&
    debug2 "$(printf "diff_select()\n   dir1=%s\n   dir2=%s\n  path=%s\n" "$1" "$2" "$3")"
  local relpath="$3"
  local fullpath="$1/$3"
  if test -d "$fullpath"
  then diff_dir_recursive "$1" "$2" "$relpath/"
    # TODO(fangism): what about test -L for symlinks?
  else diff_file_relpath "$1" "$2" "$relpath"
  fi
}

function diff_dir_recursive() {
  # $1 and $2 are two directories, e.g. "out/default"
  # $3 is the relative path down from $1 and $2, e.g. "subdir/" or "".
  # For dual-traversal, arbitrarily use $2's subdirs.
  # echo "Comparing: $sub"
  local sub="$3"  # sub-dir or file

  # Ignore some dirs.
  case "$sub" in
    # Ignore files whose names are content-hash like.
    amber-files/repository/blobs/) return ;;
    amber-files/repository/targets/) return ;;

    # Bazel runfiles trees are symlinks to test inputs (mostly sources and
    # prebuilts, plus outputs that are compared where they're built), some of
    # them absolute, so ignore those. diff_select() passes directories with a
    # trailing slash.
    *.runfiles/) return ;;
    *) ;;  # continue
  esac

  for f in "$2/$sub"*
  do
    filebase="${f##*/}"  # basename
    relpath="$sub$filebase"
    diff_select "$1" "$2" "$relpath"
  done
}

diff_output_file="$(mktemp)"
readonly diff_output_file
# Scratch space for the prepass below.
work_dir="$(mktemp -d)"
readonly work_dir
trap 'rm -rf "$diff_output_file" "$work_dir"' EXIT
# Background jobs in a non-interactive shell ignore SIGINT, so kill them
# explicitly to make Ctrl-C stop the prepass.
trap 'trap - INT TERM; kill 0 2>/dev/null; exit 130' INT TERM

# Make globs over empty dirs expand to nothing, so diff_dir_recursive()
# doesn't need to spawn `ls` per directory to detect them.
shopt -s nullglob

ARGS=()
while [[ "$#" -ne 0 ]]; do
  case "$1" in
    --verbose)
      DEBUG_LEVEL=$(( DEBUG_LEVEL + 1 ))
      ;;
    --help)
      usage
      exit 0
      ;;
    -*)
      echo >&2 "ERROR: Unknown option, see --help."
      exit 1
      ;;
    *)
     ARGS+=("$1")
     ;;
  esac
  shift
done

# Spawning a diff tool for each of the ~120k mostly identical files dominated
# the run time, so hash both trees up front in parallel and let
# diff_file_relpath() skip files whose hashes match.
declare -A left_hashes=()
declare -A right_hashes=()
# Likewise, `file` was spawned for each of the ~20k ELF binaries to classify
# them, so run `file` over the left tree up front too and keep the binary
# types.
declare -A left_types=()

function parallel_over_tree() {
  # $1 is a directory, $2 is the path under it ("." or RELPATH), $3 is the
  # output file, and the rest is a command to run on batches of file paths.
  # Skip the same directories that diff_dir_recursive() skips, and follow
  # symlinks like the comparison does. Each batch writes to its own file
  # because parallel writers to one file interleave their records.
  local -r dir="$1" start="$2" out="$3"
  shift 3
  local batch_dir
  batch_dir="$(mktemp -d -p "$work_dir")" || return
  (cd "$dir" &&
    find -L "$start" \
      \( -type d \( -name '*.runfiles' \
        -o -path '*/amber-files/repository/blobs' \
        -o -path '*/amber-files/repository/targets' \) -prune \) \
      -o -type f -print0 2>/dev/null |
    xargs -0 -r -P "$jobs" -n 256 \
      sh -c 'out="$(mktemp -p "$0")" && exec "$@" > "$out" 2>/dev/null' \
      "$batch_dir" "$@")
  # Not a glob: with nullglob and no batches, `cat` would read stdin.
  find "$batch_dir" -type f -exec cat {} + > "$out"
}

function load_types() {
  # $1 is a file of `file -N -0` output ("PATH\0: TYPE\n" records). Only keep
  # the binary types that diff_file_relpath() looks up, to keep the array
  # small.
  local path type
  while IFS= read -r -d '' path && IFS= read -r type
  do
    type="${type#: }"
    path="${path#./}"
    [[ -n "$path" ]] || continue
    case "$type" in
      ELF* | Mach-O*) left_types["$path"]="$type" ;;
    esac
  done < "$1"
}

function load_hashes() {
  # $1 is a file of `sha1sum -z` output, $2 is the name of an associative
  # array to fill with relative path -> hash.
  local -n hashes="$2"
  local entry path
  while IFS= read -r -d '' entry
  do
    # Each record is a 40-character hash, a two-character separator ("  " in
    # text mode or " *" in binary mode), and the path.
    path="${entry:42}"
    path="${path#./}"
    [[ -n "$path" ]] || continue
    hashes["$path"]="${entry:0:40}"
  done < "$1"
}

if [[ "${#ARGS[@]}" -ge 2 ]]
then
  # Three passes run at once, so split the CPUs between them.
  jobs="$(( $(nproc 2>/dev/null || echo 3) / 3 ))"
  [[ "$jobs" -ge 1 ]] || jobs=1
  hash_start="${ARGS[2]:-.}"
  parallel_over_tree "${ARGS[0]}" "$hash_start" "$work_dir/left.sha1" sha1sum -z -- &
  parallel_over_tree "${ARGS[1]}" "$hash_start" "$work_dir/right.sha1" sha1sum -z -- &
  parallel_over_tree "${ARGS[0]}" "$hash_start" "$work_dir/left.types" file -N -0 -- &
  wait
  load_hashes "$work_dir/left.sha1" left_hashes 2>/dev/null
  load_hashes "$work_dir/right.sha1" right_hashes 2>/dev/null
  load_types "$work_dir/left.types" 2>/dev/null
  # Without these, comparisons still work but are much slower, so make a
  # broken prepass visible.
  echo "Prepass: hashed ${#left_hashes[@]} + ${#right_hashes[@]} files, found ${#left_types[@]} binaries"
fi

case "${#ARGS[@]}" in
  2)
    diff_dir_recursive "${ARGS[@]}"
    ;;
  3)
    diff_select "${ARGS[@]}"
    ;;
  *)
    usage
    exit 2
esac

# Summarize findings:
echo "======== COMPARISON SUMMARY ========"
exit_status=0

# Informational findings come first so the fatal ones are at the end of the
# log, next to the exit status.
echo "---- INFORMATIONAL FINDINGS (these don't fail the comparison) ----"

# Good news: these files matched. A match is never a determinism problem,
# only a classification that's looser than it needs to be, and diff
# classifications are mostly by file name, so new files that share a name
# with a known-diffing output often show up here. Report them without
# failing so they can be cleaned up without blocking anyone.
if test "${#unexpected_matches[@]}" != 0
then
  echo "UNEXPECTED MATCHES: (action: make these expect=\"match\" now?)"
  for path in "${unexpected_matches[@]}"
  do echo "  $path"
  done
  echo "end of UNEXPECTED MATCHES"
  echo
  # Leave exit status as it were.
fi

# This group mismatched, but we didn't know what to expect.
if test "${#unclassified_diffs[@]}" != 0
then
  echo "UNCLASSIFIED DIFFS: (action: classify them as expect=\"diff\")"
  for path in "${unclassified_diffs[@]}"
  do echo "  $path"
  done
  echo "end of UNCLASSIFIED DIFFS"
  echo
  # Leave exit status as it were.
fi

function print_long_list() {
  # $1 is the section title, $2 is the suggested action, and the rest of the
  # args are the file paths.
  local -r title="$1"
  local -r action="$2"
  shift 2
  local count="$# files"
  [[ "$#" == 1 ]] && count="1 file"
  echo "$title: $count (action: $action)"
  # These lists are long and rarely useful, so only print them if verbose.
  if [[ "$DEBUG_LEVEL" -ge 1 ]]
  then
    for path in "$@"
    do echo "  $path"
    done
    echo "end of $title"
  else
    hidden_lists=1
  fi
  echo
}

hidden_lists=0

# This group matched, but we didn't know what to expect.
if test "${#unclassified_matches[@]}" != 0
then
  print_long_list "UNCLASSIFIED MATCHES" 'classify them as expect="match"' \
    "${unclassified_matches[@]}"
fi

if test "${#skipped_files[@]}" != 0
then
  print_long_list "SKIPPED FILES" \
    "compare these after all other differences have been resolved" \
    "${skipped_files[@]}"
fi

if test "${#missing_files[@]}" != 0
then
  print_long_list "FILES IN ONLY ONE DIR" \
    "make sure the builds produce the same set of files" \
    "${missing_files[@]}"
fi

# Don't bother reporting "${ignored_files[@]}", the list is long and not useful.

if [[ "$hidden_lists" == 1 ]]
then
  echo "Rerun with --verbose to list all files and show all diffs."
fi

echo
echo "---- FATAL FINDINGS (these fail the comparison) ----"
if test "${#unexpected_diffs[@]}" != 0
then
  echo "UNEXPECTED DIFFS: (action: fix source of difference)"
  for path in "${unexpected_diffs[@]}"
  do echo "  $path"
  done
  echo "end of UNEXPECTED DIFFS"
  echo
  exit_status=1
else
  echo "No UNEXPECTED DIFFS found (good)"
fi

# Make sure all cases are covered.
if test "${#unspecified_files[@]}" != 0
then
  echo "UNSPECIFIED FILES: (action: classify into {match,diff,unknown,ignore,skip})"
  for path in "${unspecified_files[@]}"
  do echo "  $path"
  done
  echo "end of UNSPECIFIED FILES"
  echo
  exit_status=1
fi

[[ "$exit_status" == 0 ]] ||
  echo "ACTION: investigate the above, perhaps file a go/fuchsia-build-bug."

echo "Exiting with status $exit_status"
exit "$exit_status"
