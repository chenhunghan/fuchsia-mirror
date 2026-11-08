# Fuchsia WLAN Testing

| Test Suite | Scope | Environment | Guide |
| :--- | :--- | :--- | :--- |
| **Crate Unit Tests** | Algorithms, frame parsers, crypto handshakes, and state machines in `lib/*`, `wlancfg`, `wlanix`, `wlandevicemonitor` | Host or hermetic component sandbox | [`../lib/README.md`](../lib/README.md) |
| **WLAN Core** | Core stack components without policy layer | Hardware testbed | [`core/README.md`](core/README.md) |
| **WLAN Policy** | Policy behaviors, auto-connect, network selection, roaming | Hardware testbed | [`wlancfg/`](wlancfg/) |
| **WLANIX** | Android WiFi HAL implementation for Starnix | Hardware testbed | [`wlanix/README.md`](wlanix/README.md) |
| **E2E Automation** | Executing and debugging test suites | Local / remote testbeds | [E2E Skill](../.agents/skills/wlan-run-e2e-test/SKILL.md) |

## End-to-End Testing

WLAN end-to-end tests are host-driven [Lacewing](../../../testing/end_to_end/README.md)
tests and require the device under test to be flashed with the corresponding
testing product bundle (`for-testing-wlan-core.<board>`,
`for-testing-wlan-platform.<board>`, or `for-testing-wlan-wlanix.<board>`).

For DUT-only tests, add the host test target and run it like any other Fuchsia
test via `fx test`:

```sh
fx add-host-test //src/connectivity/wlan/tests/core:phy_existence_test
fx test --build --e2e --output //src/connectivity/wlan/tests/core:phy_existence_test
```

For tests that require an Access Point (reachable over SSH directly or via port
forwarding), pass the Access Point SSH parameters after `--`:

```sh
fx add-host-test //src/connectivity/wlan/tests/core:connect_to_ap_test
fx test --build --e2e --output //src/connectivity/wlan/tests/core:connect_to_ap_test -- \
  --ap-ip <IP address> --ap-ssh-port <port>
```

* Passing `--ap-ip` generates an `OpenWrtAP` controller configuration by
  default.
* `--ap-ssh-port` defaults to `22` if omitted.
* `--ap-ssh-key <path>` can be passed to specify a custom SSH private key for
  the Access Point (if omitted, `mobly_driver` searches
  `~/.ssh/openwrt_testing_rsa` and `~/.ssh/testing_rsa`).

### Filtering Individual Test Cases

<!-- TODO(https://fxbug.dev/581806427): Update to use `fx test --test-filter` once glob patterns and parameterized pre_run are supported for Mobly tests. -->
To run a subset of test cases within a test suite, pass `--test_cases` after
`--`:

```sh
fx test --build --e2e --output //src/connectivity/wlan/tests/core:connect_to_ap_test -- \
  --ap-ip <IP address> --ap-ssh-port <port> \
  --test_cases 're:.*SecurityOpen.*'
```
