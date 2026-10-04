# driver-lab proxy driver

This directory contains the `driver-lab` proxy driver, a standalone driver
that can claim device nodes and expose them to host-side scripting at run-time
for rapid driver development and debugging.

See the [specification](./SPEC.md) for more details.

**Note:** This code is experimental and has not been subject to the standard
level of human code review.

---

Engineering-only DFv2 driver (`lab_proxy`) providing policy-checked,
audited hardware access for host-driven driver development. Phase 1
offers read-only access and serves the `fuchsia.driver.lab` wire
contract consumed by the host tooling at `//tools/driver-lab`. The
proxy binds only to nodes explicitly marked with the
`fuchsia.driver.lab.PROXY_TARGET` property -- no production node carries
it, so the proxy never binds opportunistically.

Every operation is validated against the immutable target ceiling and
the session's exact allowlist immediately before access, and every
attempt -- including rejections -- is recorded in a bounded audit ring.
The proxy must never be offered by production images.

The normative specification is [SPEC.md](SPEC.md); the companion host
tooling specification is at `//tools/driver-lab/SPEC.md`.

## Testing

Host-side unit tests for the policy/audit/executor core:

```
$ fx test --host lab_proxy_core_lib_test
```

Target unit tests and driver realm tests:

```
$ fx test lab_proxy-unit-test lab_proxy-realm-test
```
