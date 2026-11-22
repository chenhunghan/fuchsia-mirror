# HLCPP to Natural C++ semantic differences and review checklist

Wire-format encoding and decoding are identical between HLCPP and Natural C++.
Nearly all migration regressions arise from lifecycle, dispatcher, callback
cancellation, and channel-teardown differences. Use this reference when porting
stateful servers/clients or reviewing a migration CL.

## 1. Behavioral differences reference

### Threading and dispatchers

| ID | Topic | HLCPP behavior | Natural C++ behavior | Migration risk |
|---|---|---|---|---|
| a1 | Default dispatcher | `dispatcher = nullptr` uses `async_get_default_dispatcher()` on the current thread. | Dispatcher argument is mandatory and asserted non-null (`ZX_ASSERT`). | Naming the wrong loop moves dispatch across threads; constructing on thread A and binding on loop B panics. |
| a2 | Thread affinity | `InterfacePtr` and `Binding` are thread-hostile but unenforced in release builds (cross-thread calls are silent UB). | `fidl::Client` and `fidl::ServerBinding` enforce a synchronized dispatcher at runtime (`ZX_PANIC` on cross-thread use). | Latent cross-thread calls crash immediately. Do not paper over with `SharedClient` / `WireSharedClient` without two-phase async teardown. |
| a3 | In-flight calls on client destruction | `~InterfacePtr` drops pending response callbacks silently. | `.Then(cb)` is canceled silently on client destruction; `.ThenExactlyOnce(cb)` **always** fires with `ZX_ERR_CANCELED` (`fidl::Reason::kUnbind`). | Using `.ThenExactlyOnce` while capturing `this` on an owner-member client causes use-after-free when the owner dies. |
| a4 | Error delivery timing | Send-side validation/write errors invoke `error_handler` synchronously and re-entrantly (`ZX_ERR_PEER_CLOSED` on write is ignored). Receive errors run inline on the dispatcher. | `on_fidl_error` and `CloseHandler` are **always asynchronous** on the dispatcher, except during `loop.Shutdown()` when they run synchronously. | Code assuming synchronous error callbacks inside a method call sees an extra loop turn; tests resetting a client need `RunLoopUntilIdle()`. |
| a5 | Message batching | One wait wakeup drains up to `signal->count` queued messages before re-arming. | Dispatches one message per wakeup and re-arms (round-robin across channels). | Tests or components relying on cross-channel ordering bursts can see different message interleaving. |

### Teardown ordering and completers

| ID | Topic | HLCPP behavior | Natural C++ behavior | Migration risk |
|---|---|---|---|---|
| b1 | Server binding destruction | `~Binding` unbinds synchronously and never invokes the error handler. | `~ServerBinding` unbinds synchronously on the synchronized dispatcher and discards `CloseHandler`. `ServerBindingRef::Unbind()` is **asynchronous**. | Using `BindServer` + `ServerBindingRef` without deferring impl destruction to `OnUnbound` causes mid-dispatch UAF. Prefer `ServerBinding` or `BindServer(..., std::unique_ptr<Impl>)`. |
| b2 | `Close(epitaph)` | `Binding::Close(status)` writes the epitaph immediately, unbinds synchronously, and does **not** invoke the error handler. | `ServerBinding::Close(epitaph)` initiates async teardown, writes the epitaph at teardown end, and **invokes** `CloseHandler` with `fidl::Reason::kClose`. | Close handlers that treat every call as an unexpected peer error mis-log/mis-count; state cannot be reused before teardown completes. |
| b3 | `CloseAll` and empty-set handler | `BindingSet::CloseAll()` destroys bindings synchronously and fires neither per-binding error handlers nor `empty_set_handler`. | `ServerBindingGroup::CloseAll()` tears down asynchronously, fires each binding's `CloseHandler`, then fires the empty-set handler. | Shutdown sequences combining `CloseAll()` with an "exit when empty" callback can double-exit or exit mid-teardown. |
| b4 | Dropped reply callback / completer | Dropping a reply callback silently leaves the two-way call unanswered. | Dropping an unanswered `Completer::Async` closes the channel with `ZX_ERR_BAD_STATE` (`fidl::Reason::kAbandonedAsyncReply`); dropping a `Sync` completer panics. | Hanging-get servers that overwrite a stored waiter or erase a pending entry without replying now disconnect the client. |
| b5 | Dispatcher shutdown | Live bindings run error handlers synchronously on the shutting-down thread with `ZX_ERR_CANCELED`. | Same, surfaced with `fidl::Reason::kDispatcherError` (`SharedClient` teardown observers also run there). | Handlers switching on raw `zx_status_t` should use `fidl::UnbindInfo` reason helpers instead. |

### Error handling, epitaphs, and validation

| ID | Topic | HLCPP behavior | Natural C++ behavior | Migration risk |
|---|---|---|---|---|
| c1 | Peer-closed fan-out | Only `error_handler` runs (receives epitaph status or `ZX_ERR_PEER_CLOSED`). | Both `on_fidl_error` **and** every in-flight `.Then(...)` callback receive the peer-closed / epitaph error (`N + 1` notifications). | Calling shared cleanup or logging disconnect metrics from both `.Then` error branches and `on_fidl_error` causes double-handling. |
| c2 | Local failure status codes | Decode -> `ZX_ERR_INVALID_ARGS`; unknown ordinal -> `ZX_ERR_NOT_SUPPORTED`; bad magic -> `ZX_ERR_PROTOCOL_NOT_SUPPORTED`. | Categorized via `fidl::Reason` (`kDecodeError`, `kUnexpectedMessage`, `kEncodeError`, `kUnbind`, etc.) with `is_peer_closed()` / `is_canceled()` helpers. | Inspect counters, logs, or branches matching exact `zx_status_t` values can drift unless migrated to `fidl::Reason` / `is_peer_closed()`. |
| c3 | Automatic epitaphs | Never sent automatically (only explicit `Close(status)`). | Same (`Close(epitaph)` and `completer.Close(status)` send epitaphs; decode/encode errors do not). | Parity, aside from b4 and c4 closing the channel. |
| c4 | Server reply encode failure | Keeps the channel open when a reply exceeds byte/handle limits or has invalid handle rights (https://fxbug.dev/42083300). | Tears down the server binding on reply encode/send failure. | Servers occasionally returning oversized vectors or wrong-rights handles now disconnect the client. |
| c5 | Server receiving epitaph | Accepted without closing. | Rejected as invalid message -> tears down binding. | Only affects malformed peers. |
| d1 | Unknown interactions (`open`/`ajar`) | Pure-virtual `handle_unknown_method(ordinal, ...)`; closed protocols unbind with `ZX_ERR_NOT_SUPPORTED`. | `handle_unknown_method(fidl::UnknownMethodMetadata<P>, UnknownMethodCompleter::Sync&)`; closed protocols unbind with `Reason::kUnexpectedMessage`. | Parity; verify `hlcpp_conversion.h` builds cleanly when bridging flexible two-way methods. |
| e1 | Header and magic validation | Skips header check on empty two-way payloads (https://fxbug.dev/42082198) and magic-number check on responses/events (https://fxbug.dev/42080230). | Strictly validates headers and magic numbers -> tears down on mismatch. | Stricter on receive; only affects peers sending malformed raw messages. |
| e2 | Transaction ID validation | Rejects one-way with non-zero txid or two-way with zero txid. | Does not reject mismatched txid zero/non-zero on server receive (https://fxbug.dev/42080225). | Laxer; no impact on well-formed peers. |

### Domain objects, calls, events, and memory

| ID | Topic | HLCPP behavior | Natural C++ behavior | Migration risk |
|---|---|---|---|---|
| e4 | Flexible enums and unions | `IsUnknown()` / `UnknownBytes()`. | `IsUnknown()` and `Which() == Tag::_do_not_handle_this__write_a_default_case_instead` (tables hide unknown ordinals). | Every `switch` over a flexible enum or union must include a `default:` branch. |
| e5 | Table field accessors | `has_x()` + `x()` (asserts presence via `ZX_ASSERT`). | `x()` returns `std::optional<T>&`; `*x()` or `x().value()` on `std::nullopt` is UB/abort. | Always check `.has_value()` or use `.value_or(...)` before dereferencing. |
| e6 | Optional strings and vectors | `fidl::StringPtr` and `fidl::VectorPtr<T>` distinguish null vs empty (`""` / `[]`). | `std::optional<std::string>` and `std::optional<std::vector<T>>`. | Preserve null (`std::nullopt`) vs empty (`""` / `{}`) when rewriting `VectorPtr::reset()` or implicit conversions. |
| e7 | Copyability and `fidl::Clone` | Resource and non-trivial types use `fidl::Clone` (which duplicates handles) or `std::move`. | Value types are copyable; resource types (with handles) are move-only and have no `fidl::Clone`. | Replacing `fidl::Clone` with `std::move` transfers handle ownership instead of duplicating; conversely, stale `std::move` on value tables can leave cached members empty. |
| f1 | One-way client calls | Returns `void`; `ZX_ERR_PEER_CLOSED` on write is silently swallowed. | Returns `[[nodiscard]] fit::result<fidl::OneWayError>`; any send error also schedules async unbind. | `ZX_ASSERT(res.is_ok())` in production crashes on normal peer-closed races; log only when `res.error_value().status() != ZX_ERR_PEER_CLOSED`. |
| g1 | Synchronous client calls | `SynchronousInterfacePtr` blocks indefinitely (`ZX_TIME_INFINITE`), ignores epitaphs, and stays bound after errors. | `fidl::SyncClient` blocks indefinitely, surfaces epitaph/reason via `error_value()`, drains events via `HandleOneEvent`, and stays bound after errors. | Replacing an async `InterfacePtr` with `SyncClient` on a shared server/VFS thread deadlocks. |
| h1 | Event handler registration | Per-event lambdas (`ptr.events().OnFoo = ...`) can be attached after `Bind()`. | `fidl::AsyncEventHandler<P>*` must be supplied at `Bind()` time. | Binding early and setting up event handling later is impossible; pass the handler to `Bind()`. |
| i1 | `BindingSet` vs `ServerBindingGroup` | Exposes live `bindings()` container; `RemoveBinding` / `CloseAll` do not fire error handlers. | Exposes `size()`, `ForEachBinding`, `RemoveBindings` (silent), and `CloseBindings` / `CloseAll` (async, fires `CloseHandler` + empty-set handler). | Choose `RemoveBindings` vs `CloseBindings` deliberately depending on whether close handlers and epitaphs should run. |
| j1 | Re-entrancy and sync waiting | Supports `WaitAndDispatchOneMessageUntil`; send errors can re-enter caller synchronously. | Never invokes user callbacks synchronously from `Client` / `ServerBinding` calls; no `WaitAndDispatch...` equivalent. | Replace `WaitAndDispatchOneMessageUntil` with async callbacks; defer self-erasure in close handlers via `async::PostTask` if needed. |
| k1 | Server `Request&` lifetime | Request parameters passed by value (owned by callee). | `Request&` borrows from the binding's decode buffer for the synchronous handler call only. | Keeping pointers or references into `Request&` across `completer.ToAsync()` causes a use-after-free. Move/copy fields first. |

## 2. 20-item migration review checklist

1.  No `Bind(...)`, `fidl::Client`, or `fidl::ServerBinding` uses
    `async_get_default_dispatcher()` unless the owning class documents
    single-loop thread affinity (a1).
1.  Every `.ThenExactlyOnce(...)` captures only self-owned/shared state or lives
    in a test that awaits completion before teardown; member-client callbacks
    capturing `this` use `.Then(...)` (a3).
1.  No `ZX_ASSERT`, `FX_CHECK`, or `.value()` on production one-way call results
    to peers that can close; ignore or filter `status() == ZX_ERR_PEER_CLOSED` (f1).
1.  Error and close handlers do not assume synchronous execution inside send
    calls, and do not destroy the active container/binding unsafely without
    `async::PostTask` when needed (a4, j1).
1.  Error handlers that previously switched on `zx_status_t` use
    `fidl::UnbindInfo::reason()`, `is_peer_closed()`, or `is_canceled()`, and
    published Inspect/metrics error keys remain stable (c2).
1.  Every `Close(epitaph)` call site accounts for async teardown and a
    `CloseHandler` invocation with `fidl::Reason::kClose` (b2).
1.  `ServerBindingGroup::CloseAll` users with an empty-set handler verify
    shutdown ordering now that `CloseAll` triggers the empty-set callback (b3).
1.  Every two-way server method calling `completer.ToAsync()` checks all paths
    that could drop or overwrite the `Completer::Async` (for example, duplicate
    hanging-get requests or map-entry erasure) (b4).
1.  No `fidl::ServerBindingRef` is used without an `OnUnbound` hook owning
    implementation destruction; `fidl::ServerBinding` or `fidl::BindServer(...,
    std::make_unique<Impl>(...))` is preferred (b1).
1.  No `fidl::SharedClient` or `fidl::WireSharedClient` is introduced merely to
    bypass thread-affinity assertions without two-phase teardown (a2).
1.  `fidl::AsyncEventHandler<P>` is passed at `Bind` time and declared before
    the `fidl::Client<P>` member so it outlives the client (h1).
1.  Removed `fidl::Clone` calls on resource types duplicate handles explicitly
    (`ZX_RIGHT_SAME_RIGHTS`), and leftover `std::move` calls on cached value
    tables are audited (e7).
1.  Every `has_x()` -> `x().has_value()` conversion guards against dereferencing
    an absent `std::optional` (e5).
1.  Every `switch` over flexible enums or unions has a `default:` branch (e4).
1.  No pointer or reference into a server `Request&` outlives the synchronous
    handler frame across `completer.ToAsync()` (k1).
1.  `WaitAndDispatchOneMessageUntil` patterns are replaced with event-loop flow,
    and `ASSERT_*` inside `.Then(...)` test lambdas cannot skip `QuitLoop()` /
    latch countdowns (j1).
1.  Server replies returning large vectors or handles are checked against
    channel limits now that encode failures tear down the binding (c4).
1.  Incoming `/svc` dependencies in libraries/classes take
    `fidl::UnownedClientEnd<fuchsia_io::Directory>` (or `ClientEnd`) +
    `component::ConnectAt<P>` rather than hardcoding `component::Connect<P>()`
    outside `main()`.
1.  Tests use `fidl::Endpoints<P>::Create()`, `LocalCppComponent` (for
    `RealmBuilder`), or `ServiceDirectoryProvider` rather than unnecessary HLCPP
    channel bridges.
1.  Repeat runs (`fx test --count 20`) and sanitizer builds where applicable are
    executed to catch async unbind/teardown races.
