# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import functools
from collections.abc import Mapping, Sequence
from typing import Any, ClassVar, Generic, TypeVar

from pydantic import (
    AliasChoices,
    BaseModel,
    ConfigDict,
    TypeAdapter,
    field_serializer,
    field_validator,
)

PROTOCOL_VERSION = 23

THREAD_ID_ALIAS = AliasChoices("thread_id", "threadId")

T_Resp = TypeVar("T_Resp")


def _normalize_event_thread_ids(
    events: Sequence[Mapping[str, Any]] | None,
) -> list[dict[str, Any]] | None:
    if events is None:
        return None
    normalized: list[dict[str, Any]] = []
    for event in events:
        body = event.get("body")
        if isinstance(body, Mapping) and "threadId" in body:
            thread_id = body.get("thread_id", body["threadId"])
            normalized_body = {
                ("thread_id" if key == "threadId" else key): (
                    thread_id if key in ("threadId", "thread_id") else value
                )
                for key, value in body.items()
            }
            normalized.append(dict(event) | {"body": normalized_body})
        else:
            normalized.append(dict(event))
    return normalized


class BaseRequest(BaseModel, Generic[T_Resp]):
    """Base class for all requests.

    Pydantic models only accept keyword arguments, so instantiation is already
    keyword-only. Requests are frozen (read-only) so that subclasses can
    narrow `command` and `last_seen_seq` in a type-safe way; use
    `model_copy(update=...)` to derive a modified request.
    """

    model_config = ConfigDict(frozen=True, populate_by_name=True)

    command: str
    last_seen_seq: int | None = None
    ack_seq: int | None = None
    response_type: ClassVar[Any] = dict[str, Any]


class Response(BaseModel, Generic[T_Resp]):
    """Standard response wrapper."""

    model_config = ConfigDict(validate_assignment=True)

    success: bool
    message: str | None = None
    body: T_Resp | None = None
    events: list[dict[str, Any]] | None = None

    @field_validator("events", mode="before")
    @classmethod
    def _normalize_events(
        cls, events: Sequence[Mapping[str, Any]] | None
    ) -> list[dict[str, Any]] | None:
        return _normalize_event_thread_ids(events)

    @field_serializer("events")
    def _serialize_events(
        self, events: Sequence[Mapping[str, Any]] | None
    ) -> list[dict[str, Any]] | None:
        return _normalize_event_thread_ids(events)


class ProtocolRegistry:
    request_adapter: TypeAdapter[Any] | None = None
    response_adapter: TypeAdapter[Any] | None = None


def serialize(obj: BaseModel) -> str:
    return obj.model_dump_json() + "\n"


def make_request(data: dict[str, Any]) -> BaseRequest[Any]:
    if ProtocolRegistry.request_adapter is None:
        raise RuntimeError("ProtocolRegistry not initialized")
    return ProtocolRegistry.request_adapter.validate_python(data)


def deserialize_request(line: str) -> BaseRequest[Any]:
    if ProtocolRegistry.request_adapter is None:
        raise RuntimeError("ProtocolRegistry not initialized")
    return ProtocolRegistry.request_adapter.validate_json(line.strip())


@functools.lru_cache(maxsize=32)
def _get_response_adapter(resp_type: Any) -> TypeAdapter[Any]:
    """Build and cache a TypeAdapter for the specialized Response[resp_type].

    Constructing a TypeAdapter compiles Pydantic's core validation graph.
    Caching by response_type allows reuse across responses.
    """
    target_cls: Any = Response[resp_type]
    return TypeAdapter(target_cls)


def deserialize_response(
    line: str, req: BaseRequest[T_Resp]
) -> Response[T_Resp]:
    """Deserialize and validate a raw JSON response against req's response_type."""
    return _get_response_adapter(req.response_type).validate_json(line.strip())


def get_schema() -> dict[str, Any]:
    if (
        ProtocolRegistry.request_adapter is None
        or ProtocolRegistry.response_adapter is None
    ):
        raise RuntimeError("ProtocolRegistry not initialized")
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "zxdb-cli Protocol Schema",
        "description": (
            "JSON schema for requests and responses in the "
            "zxdb-cli UDS protocol"
        ),
        "version": PROTOCOL_VERSION,
        "requests": ProtocolRegistry.request_adapter.json_schema(),
        "responses": ProtocolRegistry.response_adapter.json_schema(),
    }
