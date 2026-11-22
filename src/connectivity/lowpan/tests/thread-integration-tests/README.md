# thread-integration-tests

Hermetic multi-node Thread integration tests.

* `common/`: the fixture, crate `thread_testing_common`. It runs Thread nodes in netemul realms on
  a simulated radio medium; see the crate documentation in `common/src/lib.rs` for the topology.
* Every other directory is a suite of tests using the fixture, in a package of its own:
  * `formation/`: a single Thread node (Thread Border Router or Thread-only) boots and forms a
    network.

```
fx test thread-formation-integration-test
```

## Adding a suite

Add a directory with a `rustc_test` depending on `common:thread_testing_common` and a
`fuchsia_unittest_package` whose `subpackages` is `thread_test_subpackages` from
`common/thread_test_package.gni` (`expect_includes` adds `common/client.shard.cml` to the generated
manifest automatically). See `formation/BUILD.gn`. List it in `BUILD.gn`.

## Limitations

* `ot-rcp-fuchsia` does not generate or process IEEE 802.15.4 acknowledgments yet
  ([fxbug.dev/569784945](https://fxbug.dev/569784945)), so unicast transmissions fail with `NO_ACK`:
  only single-node scenarios work.
* Some capabilities lowpan-ot-driver uses are left unrouted; see `DRIVER_PROTOCOLS_FROM_NETSTACK`
  in `common/src/node.rs`.
