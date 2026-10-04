# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for grant models, resolution, and the persistent store."""

import tempfile
import unittest
from pathlib import Path
from typing import Any

from driver_lab.models import AccessClass, AccessRequest, Decision, ReadGrant
from driver_lab.permissions import (
    GrantStoreError,
    Outcome,
    add_grant,
    load_grants,
    resolve,
    revoke_grant,
    save_grants,
)

DIGEST = "sha256:" + "ab" * 32


def make_grant(**overrides: object) -> ReadGrant:
    fields: dict[str, Any] = dict(
        schema_version=1,
        target_scope="example-engineering-target",
        node_id="example-device",
        resource_digest=DIGEST,
        resource="control",
        offset=0x3C,
        width=4,
        access=AccessClass.READ_ONCE,
        decision=Decision.ALLOW,
        approved_at="2026-07-30T00:00:00Z",
        reason="reviewed against the register specification",
    )
    fields.update(overrides)
    return ReadGrant(**fields)


def make_request(**overrides: object) -> AccessRequest:
    fields: dict[str, Any] = dict(
        target_scope="example-engineering-target",
        node_id="example-device",
        resource_digest=DIGEST,
        resource="control",
        offset=0x3C,
        width=4,
        access=AccessClass.READ_ONCE,
    )
    fields.update(overrides)
    return AccessRequest(**fields)


class ResolveTest(unittest.TestCase):
    def test_exact_match_allows(self) -> None:
        resolution = resolve(make_request(), [make_grant()])
        self.assertIs(resolution.outcome, Outcome.ALLOWED)
        self.assertEqual(resolution.grant, make_grant())

    def test_no_match_is_undecided_and_fails_closed(self) -> None:
        resolution = resolve(make_request(), [])
        self.assertIs(resolution.outcome, Outcome.UNDECIDED)
        self.assertFalse(resolution.decided)

    def test_every_identity_field_must_match(self) -> None:
        mismatches = [
            make_request(offset=0x40),
            make_request(width=8),
            make_request(resource="status"),
            make_request(node_id="other-device"),
            make_request(target_scope="other-target"),
            make_request(resource_digest="sha256:" + "cd" * 32),
        ]
        for request in mismatches:
            resolution = resolve(request, [make_grant()])
            self.assertIs(resolution.outcome, Outcome.UNDECIDED, request)

    def test_access_classes_are_distinct(self) -> None:
        grant = make_grant()  # READ_ONCE
        self.assertIs(
            resolve(make_request(access=AccessClass.SNAPSHOT), [grant]).outcome,
            Outcome.UNDECIDED,
        )
        self.assertIs(
            resolve(make_request(access=AccessClass.POLL), [grant]).outcome,
            Outcome.UNDECIDED,
        )

    def test_boot_change_does_not_invalidate(self) -> None:
        # Grants record no boot identity: the same stable identity resolves
        # identically across reboots. A resource-description change appears
        # as a digest mismatch instead.
        self.assertIs(
            resolve(make_request(), [make_grant()]).outcome, Outcome.ALLOWED
        )

    def test_deny_takes_precedence_over_allow(self) -> None:
        allow = make_grant()
        deny = make_grant(decision=Decision.DENY)
        # A deny and allow with the same match identity cannot both be
        # stored, but resolution still prefers deny defensively.
        resolution = resolve(make_request(), [allow, deny])
        self.assertIs(resolution.outcome, Outcome.DENIED)
        assert resolution.grant is not None
        self.assertIs(resolution.grant.decision, Decision.DENY)


class ModelValidationTest(unittest.TestCase):
    def test_write_grants_are_rejected(self) -> None:
        with self.assertRaises(ValueError):
            make_grant(access=AccessClass.WRITE)

    def test_poll_allow_requires_limits(self) -> None:
        with self.assertRaises(ValueError):
            make_grant(access=AccessClass.POLL)
        make_grant(
            access=AccessClass.POLL, max_poll_hz=10.0, max_poll_timeout_s=5.0
        )

    def test_poll_limits_only_on_poll_grants(self) -> None:
        with self.assertRaises(ValueError):
            make_grant(max_poll_hz=10.0)

    def test_digest_format_enforced(self) -> None:
        with self.assertRaises(ValueError):
            make_grant(resource_digest="not-a-digest")


class GrantStoreTest(unittest.TestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.path = Path(self._dir.name) / "grants.toml"

    def test_missing_file_is_empty_store(self) -> None:
        self.assertEqual(load_grants(self.path), [])

    def test_round_trip(self) -> None:
        grants = [make_grant(), make_grant(offset=0x40, decision=Decision.DENY)]
        save_grants(self.path, grants)
        self.assertEqual(load_grants(self.path), grants)
        # Atomic write leaves no temporary file behind.
        self.assertEqual(
            sorted(p.name for p in self.path.parent.iterdir()), ["grants.toml"]
        )

    def test_add_and_revoke(self) -> None:
        add_grant(self.path, make_grant())
        [loaded] = load_grants(self.path)
        self.assertTrue(revoke_grant(self.path, loaded.grant_id))
        self.assertEqual(load_grants(self.path), [])
        self.assertFalse(revoke_grant(self.path, loaded.grant_id))

    def test_duplicate_match_identity_rejected_on_save(self) -> None:
        save_grants(self.path, [make_grant()])
        with self.assertRaises(GrantStoreError):
            add_grant(self.path, make_grant())
        # A conflicting decision with the same identity is also ambiguous.
        with self.assertRaises(GrantStoreError):
            add_grant(self.path, make_grant(decision=Decision.DENY))

    def test_duplicate_rejected_on_load(self) -> None:
        save_grants(self.path, [make_grant()])
        text = self.path.read_text()
        block = text[text.index("[[read_grants]]") :]
        self.path.write_text(text + "\n" + block)
        with self.assertRaises(GrantStoreError):
            load_grants(self.path)

    def test_malformed_file_rejected(self) -> None:
        self.path.write_text("this is not toml [")
        with self.assertRaises(GrantStoreError):
            load_grants(self.path)

    def test_unknown_fields_rejected(self) -> None:
        save_grants(self.path, [make_grant()])
        text = self.path.read_text()
        self.path.write_text(
            text.replace("[[read_grants]]", '[[read_grants]]\nevil = "x"')
        )
        with self.assertRaises(GrantStoreError):
            load_grants(self.path)

    def test_concurrent_writer_lock(self) -> None:
        lock = self.path.with_name(self.path.name + ".lock")
        lock.write_text("held")
        with self.assertRaises(GrantStoreError):
            save_grants(self.path, [make_grant()])
        lock.unlink()
        save_grants(self.path, [make_grant()])


if __name__ == "__main__":
    unittest.main()
