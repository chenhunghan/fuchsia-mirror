# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal, Self

from pydantic import BaseModel, ConfigDict, Field, model_validator
from shared.protocol.base import BaseRequest
from shared.protocol.watch_request import WatchAccessType

COMMAND_NAME: Final = "tracepoint"

CaptureItem = Literal["frame", "backtrace", "locals", "registers"]


class TracepointInfo(BaseModel):
    """Configuration and runtime state of a non-stopping tracepoint."""

    model_config = ConfigDict(extra="forbid")

    id: int
    location: str
    file: str | None = None
    line: int | None = None
    function: str | None = None
    watch: str | None = None
    data_id: str | None = None
    size: int | None = None
    access_type: WatchAccessType | None = None
    condition: str | None = None
    expressions: list[str] = Field(default_factory=list)
    capture: list[CaptureItem] = Field(default_factory=lambda: ["frame"])
    raw_backtrace: bool = False
    max_matching_frames: int = 10
    max_hits: int = 20
    hit_count: int = 0
    enabled: bool = True
    auto_disabled: bool = False
    verified: bool = False
    message: str | None = Field(
        default=None, description="Verification or error message from DAP"
    )


class TracepointResponse(BaseModel):
    """Response for tracepoint command."""

    model_config = ConfigDict(extra="forbid")

    tracepoints: list[TracepointInfo]


def _has_target_fields(req: "TracepointRequest") -> bool:
    """Returns True if any location target field is set on the request."""
    return any(
        val is not None for val in (req.file, req.line, req.function, req.watch)
    )


def _has_non_default_options(req: "TracepointRequest") -> bool:
    """Returns True if any capture, watch, or threshold option differs from default."""
    return (
        req.size is not None
        or req.access_type != "write"
        or req.thread_id is not None
        or req.frame_index != 0
        or req.condition is not None
        or bool(req.expressions)
        or req.capture != ["frame"]
        or req.raw_backtrace
        or req.max_matching_frames != 10
        or req.max_hits != 20
    )


def _validate_bounds(req: "TracepointRequest") -> None:
    """Validates ID aliases, positive integer bounds, and non-empty strings."""
    if req.id is not None:
        if req.tracepoint_id is not None and req.tracepoint_id != req.id:
            raise ValueError("Conflicting 'id' and 'tracepoint_id' values")
        req.tracepoint_id = req.id
    if req.tracepoint_id is not None and req.tracepoint_id < 1:
        raise ValueError("tracepoint_id must be >= 1")
    if req.line is not None and req.line < 1:
        raise ValueError("line must be >= 1")
    if req.size is not None and req.size < 1:
        raise ValueError("size must be >= 1")
    if req.frame_index < 0:
        raise ValueError("frame_index must be >= 0")
    if req.max_hits < 1:
        raise ValueError("max_hits must be >= 1")
    if req.max_matching_frames < 1:
        raise ValueError("max_matching_frames must be >= 1")
    for field_name, val in (
        ("file", req.file),
        ("function", req.function),
        ("watch", req.watch),
    ):
        if val is not None and not val.strip():
            raise ValueError(f"{field_name} cannot be empty or whitespace only")


def _validate_management_mode(req: "TracepointRequest") -> bool:
    """Validates list, clear, enable/disable, and delete modes.

    Returns True when the request does not require target location validation.
    """
    mode_count = sum(
        [req.list_all, req.clear, req.delete, req.enabled is not None]
    )
    if mode_count > 1:
        raise ValueError(
            "Cannot combine list_all, clear, delete, or enabled options"
        )
    if req.list_all or req.clear:
        if (
            req.tracepoint_id is not None
            or _has_target_fields(req)
            or _has_non_default_options(req)
        ):
            raise ValueError(
                "list_all and clear cannot be combined with id, target, or"
                " capture options"
            )
        return True
    if req.enabled is not None:
        if req.tracepoint_id is None:
            raise ValueError(
                "tracepoint_id is required when enabling or disabling a"
                " tracepoint"
            )
        if _has_target_fields(req) or _has_non_default_options(req):
            raise ValueError(
                "enabled cannot be combined with target or capture options"
            )
        return True
    if req.delete:
        if _has_non_default_options(req):
            raise ValueError("delete cannot be combined with capture options")
        if req.tracepoint_id is not None:
            if _has_target_fields(req):
                raise ValueError(
                    "delete cannot combine tracepoint_id with a target location"
                )
            return True
    return False


class TracepointRequest(BaseRequest[TracepointResponse]):
    """Request to set, update, enable, disable, delete, clear, or list tracepoints.

    When hit, the backend captures requested state and emits a structured
    tracepoint event into the event stream without stopping the target thread.
    """

    command: Literal["tracepoint"] = COMMAND_NAME
    id: int | None = None
    file: str | None = None
    line: int | None = None
    function: str | None = None
    watch: str | None = None
    size: int | None = None
    access_type: WatchAccessType = "write"
    thread_id: int | None = None
    frame_index: int = 0
    condition: str | None = None
    expressions: list[str] = Field(default_factory=list)
    capture: list[CaptureItem] = Field(default_factory=lambda: ["frame"])
    raw_backtrace: bool = False
    max_matching_frames: int = 10
    max_hits: int = 20
    delete: bool = False
    clear: bool = False
    enabled: bool | None = None
    tracepoint_id: int | None = None
    list_all: bool = False
    response_type: ClassVar[Any] = TracepointResponse

    @model_validator(mode="after")
    def validate_request(self) -> Self:
        """Validates tracepoint target mode and numeric thresholds."""
        _validate_bounds(self)
        if _validate_management_mode(self):
            return self

        has_file = self.file is not None
        has_line = self.line is not None
        if has_file != has_line:
            raise ValueError(
                "Both 'file' and 'line' must be provided together for a source"
                " tracepoint"
            )

        has_source = has_file and has_line
        has_function = self.function is not None
        has_watch = self.watch is not None
        if sum([has_source, has_function, has_watch]) != 1:
            raise ValueError(
                "Must specify exactly one of ('file' and 'line'), 'function',"
                " or 'watch'"
            )

        if self.raw_backtrace and "backtrace" not in self.capture:
            self.capture.append("backtrace")

        return self
