# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generic parser and queryable model for Bazel Build Event Protocol (BEP) streams.

This module provides data structures and query methods for decoding BEP events
(target completions, output groups, and depset DAGs of files) independently of any
project-specific aspects or rules.

Specification & Reference:
    The Build Event Protocol (BEP) schema is defined in the Bazel source tree:
        src/main/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto
    and mirrored in Google Cloud / ResultStore APIs:
        google/devtools/build/v1/build_events.proto

Stream Format & Protocol Assumptions:
    1. Transport:
       Bazel writes a newline-delimited stream of JSON objects when invoked with
       `--build_event_json_file=<path>`. Each line represents a `BuildEvent`
       message serialized using standard proto3 JSON mapping rules.

    2. Event Discrimination:
       Each event dictionary has an `id` sub-dictionary specifying the event type
       (e.g. `{"namedSet": {"id": "..."}}` or `{"targetCompleted": {"label": "..."}}`).
       The corresponding payload is stored under the matching camelCase key
       (e.g. `namedSetOfFiles` or `targetCompleted`).

    3. Depset DAG (NamedSetOfFiles) Representation:
       Starlark depset objects are serialized as `NamedSetOfFiles` events. Each
       named set contains direct `files` and child `fileSets` (DAG edges). In a
       streaming BEP file, `NamedSetOfFiles` events may appear before or after the
       `TargetCompleted` events that reference them; therefore, a complete pass
       collects all named sets into a lookup table before resolving dependencies.

    4. File Path Resolution:
       The `File` message provides:
         - `name`: file name or package-relative output path.
         - `pathPrefix`: sequence of path components relative to the execroot
           (e.g. `["bazel-out", "x86_64-fastbuild", "bin"]`).
         - `uri`: fully qualified `file://` or `bytestream://` URI.
       When `pathPrefix` is non-empty, `os.path.join(*pathPrefix, name)` gives the
       exact execroot-relative path in `bazel-bin` / `bazel-out`.

    5. Proto3 Serialization Defaults:
       Under proto3 JSON mapping, fields with default values (`false` for bool,
       `0` for integers, empty strings/lists) are omitted from the JSON payload.
       Specifically, `TargetCompleted.success` is omitted when `false`.
"""

import dataclasses
import json
import os
import typing as T
from pathlib import Path

JSONObject: T.TypeAlias = dict[str, T.Any]


@dataclasses.dataclass(frozen=True)
class File:
    """Information about an output file referenced in a BEP event.

    Corresponds to message `File` in `build_event_stream.proto`.
    """

    name: str
    path_prefix: tuple[str, ...] = ()
    uri: str = ""

    @classmethod
    def from_json(cls, data: JSONObject) -> "File":
        return cls(
            name=data.get("name", ""),
            path_prefix=tuple(data.get("pathPrefix", [])),
            uri=data.get("uri", ""),
        )

    def execroot_relpath(self, execroot: Path | None = None) -> str:
        """Return the relative path from the Bazel execroot."""
        if self.path_prefix:
            return os.path.join(*self.path_prefix, self.name)
        if self.name.startswith("bazel-out/"):
            return self.name
        if self.uri.startswith("file://") and execroot:
            full_path = Path(self.uri.removeprefix("file://"))
            try:
                return str(full_path.relative_to(execroot))
            except ValueError:
                pass
        return self.name


@dataclasses.dataclass(frozen=True)
class NamedSet:
    """A named set of files (depset) in BEP.

    Corresponds to message `NamedSetOfFiles` in `build_event_stream.proto`.
    """

    id: str
    files: tuple[File, ...] = ()
    file_set_ids: tuple[str, ...] = ()

    @classmethod
    def from_json(cls, set_id: str, data: JSONObject) -> "NamedSet":
        files = tuple(File.from_json(f) for f in data.get("files", []))
        file_set_ids = tuple(
            fs.get("id", "") for fs in data.get("fileSets", []) if fs.get("id")
        )
        return cls(id=set_id, files=files, file_set_ids=file_set_ids)


@dataclasses.dataclass(frozen=True)
class OutputGroup:
    """An output group reported on target or aspect completion in BEP.

    Corresponds to message `OutputGroup` in `build_event_stream.proto`.
    """

    name: str
    file_set_ids: tuple[str, ...] = ()

    @classmethod
    def from_json(cls, data: JSONObject) -> "OutputGroup":
        return cls(
            name=data.get("name", ""),
            file_set_ids=tuple(
                fs.get("id", "")
                for fs in data.get("fileSets", [])
                if fs.get("id")
            ),
        )


@dataclasses.dataclass(frozen=True)
class TargetCompleted:
    """A targetCompleted or aspectCompleted event in BEP.

    Corresponds to message `TargetCompleted` in `build_event_stream.proto`.
    """

    label: str
    aspect: str = ""
    success: bool = False
    output_groups: tuple[OutputGroup, ...] = ()

    @classmethod
    def from_json(
        cls, tc_id: JSONObject, completed: JSONObject
    ) -> "TargetCompleted":
        label = tc_id.get("label", "")
        aspect = tc_id.get("aspect", "")
        success = completed.get("success", False)
        output_groups = tuple(
            OutputGroup.from_json(og) for og in completed.get("outputGroup", [])
        )
        return cls(
            label=label,
            aspect=aspect,
            success=success,
            output_groups=output_groups,
        )


@dataclasses.dataclass(frozen=True)
class BuildEventStream:
    """Queryable model of a parsed Bazel Build Event Protocol stream."""

    named_sets: dict[str, NamedSet] = dataclasses.field(default_factory=dict)
    targets_completed: list[TargetCompleted] = dataclasses.field(
        default_factory=list
    )
    _named_set_cache: dict[
        tuple[str, Path | None], list[str]
    ] = dataclasses.field(default_factory=dict, hash=False, compare=False)

    def resolve_named_set_files(
        self,
        set_id: str,
        execroot: Path | None = None,
        visited: set[str] | None = None,
    ) -> list[str]:
        """Recursively resolve all file paths in a NamedSet DAG."""
        cache_key = (set_id, execroot)
        if cache_key in self._named_set_cache:
            return self._named_set_cache[cache_key]

        if visited is None:
            visited = set()
        if set_id in visited:
            return []
        visited.add(set_id)

        named_set = self.named_sets.get(set_id)
        if not named_set:
            return []

        files = [f.execroot_relpath(execroot) for f in named_set.files]
        for child_id in named_set.file_set_ids:
            files.extend(
                self.resolve_named_set_files(child_id, execroot, visited)
            )
        self._named_set_cache[cache_key] = files
        return files

    def get_output_group_files(
        self,
        group_name: str,
        execroot: Path | None = None,
    ) -> list[str]:
        """Return all resolved file paths across completed targets for a given output group."""
        result: list[str] = []
        for tc in self.targets_completed:
            for og in tc.output_groups:
                if og.name == group_name:
                    for fs_id in og.file_set_ids:
                        result.extend(
                            self.resolve_named_set_files(fs_id, execroot)
                        )
        return list(dict.fromkeys(result))

    def get_target_output_group_files(
        self,
        group_name: str,
        label_predicate: T.Callable[[str], bool] | None = None,
        execroot: Path | None = None,
    ) -> list[tuple[str, str]]:
        """Return (label, file_path) pairs for completed targets matching the predicate."""
        result: list[tuple[str, str]] = []
        for tc in self.targets_completed:
            if label_predicate is not None and not label_predicate(tc.label):
                continue
            for og in tc.output_groups:
                if og.name == group_name:
                    for fs_id in og.file_set_ids:
                        for f in self.resolve_named_set_files(fs_id, execroot):
                            result.append((tc.label, f))
        return list(dict.fromkeys(result))

    @classmethod
    def from_events(cls, events: T.Iterable[JSONObject]) -> "BuildEventStream":
        """Construct a BuildEventStream from an iterable of parsed JSON event dictionaries."""
        named_sets: dict[str, NamedSet] = {}
        targets_completed: list[TargetCompleted] = []

        for event in events:
            event_id = event.get("id", {})
            if "namedSet" in event_id:
                ns_id = event_id["namedSet"].get("id", "")
                named_sets[ns_id] = NamedSet.from_json(
                    ns_id, event.get("namedSetOfFiles", {})
                )
            elif "targetCompleted" in event_id:
                targets_completed.append(
                    TargetCompleted.from_json(
                        event_id["targetCompleted"],
                        event.get("completed", {}),
                    )
                )

        return cls(named_sets=named_sets, targets_completed=targets_completed)

    @classmethod
    def from_lines(cls, lines: T.Iterable[str]) -> "BuildEventStream":
        """Construct a BuildEventStream from newline-delimited JSON strings."""

        def _event_generator() -> T.Iterator[JSONObject]:
            for line in lines:
                line = line.strip()
                if not line:
                    continue
                try:
                    yield json.loads(line)
                except json.JSONDecodeError:
                    continue

        return cls.from_events(_event_generator())

    @classmethod
    def from_file(cls, path: Path) -> "BuildEventStream":
        """Construct a BuildEventStream from a BEP JSON file on disk."""
        if not path.exists():
            return cls()
        with open(path, "rt", encoding="utf-8") as f:
            return cls.from_lines(f)
