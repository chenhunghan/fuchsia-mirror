# `binderfs` Agent Notes

## Attributing Crashes in `IPCThreadState::logExtendedError()`

`/dev/binderfs/features/extended_error` is enabled unconditionally in
`src/starnix/modules/binderfs/fs.rs` (`BinderFeaturesDir::new()`, since
Starnix commit `ac71e761834` along with `BINDER_GET_EXTENDED_ERROR`).
With this feature enabled, `IPCThreadState::logExtendedError()` in newer
Android container `libbinder` builds executes:

```cpp
LOG_ALWAYS_FATAL_IF(ee.param == -EINVAL && ee.command != BR_DEAD_REPLY,
                    "BAD: Incorrect usage of binder APIs. ...");
```

Note that Fuchsia's in-tree
`third_party/android/platform/frameworks/native/libs/binder/IPCThreadState.cpp`
is an older snapshot where `IPCThreadState::logExtendedError()` only calls
`ALOGE_IF`, while `talkWithDriver()` in the same file calls
`LOG_ALWAYS_FATAL("Driver did not consume write buffer. ...")`. That
difference reflects the newer container `libbinder` vs. the in-tree snapshot,
not a compiler artifact. (TODO(https://fxbug.dev/587285714): remove this
snapshot-mismatch note once `third_party/android/platform/frameworks/native`
is rolled to a newer AOSP release containing the `LOG_ALWAYS_FATAL_IF` check.)

When debugging crashes whose symbolized stack trace points to an inlined
`IPCThreadState::logExtendedError()` call inside `waitForResponse()` /
`transact()`, treat them as this fatal extended-error check firing for
`ee.param == -EINVAL && ee.command != BR_DEAD_REPLY`. Do not assume the
symbolized frame is a compiler tail-merging artifact from `talkWithDriver()`'s
`LOG_ALWAYS_FATAL("Driver did not consume write buffer. ...")` check, which is
what the older in-tree `IPCThreadState.cpp` would otherwise suggest when no
abort message is visible in the crash report.
