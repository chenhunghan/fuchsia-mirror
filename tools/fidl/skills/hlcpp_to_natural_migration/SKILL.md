---
name: hlcpp-to-natural-migration
description: >
  Migrate C++ components, libraries, and tests from legacy HLCPP FIDL bindings
  to Natural C++ bindings, including build dependencies, incoming and outgoing
  service wiring, support-library equivalents, and semantic lifecycle review.
---

# FIDL HLCPP to Natural C++ bindings migration

Use this skill when migrating Fuchsia C++ components, libraries, or tests from
legacy HLCPP FIDL bindings (`<fuchsia/.../cpp/fidl.h>`, `_hlcpp` targets, and
`//sdk/lib/sys/cpp`) to Natural C++ bindings (`<fidl/fuchsia.../cpp/fidl.h>`,
`_cpp` targets, and `//sdk/lib/component/{incoming,outgoing}/cpp`).

## Migration workflow

### 1. Research and choose granularity

1.  **Inventory HLCPP usage**: Find `#include <fuchsia/.../cpp/fidl.h>`,
    `#include <.../cpp/fidl_test_base.h>`, `_hlcpp` build dependencies,
    `sys::ComponentContext`, and `sys::testing::ComponentContextProvider`.
2.  **Pick all-at-once vs incremental**:
    - **Small targets**: Migrate implementation and tests together in one CL.
    - **Large targets or shared types**: Migrate one class or protocol at a time
      using `<fidl/fuchsia.foo/cpp/hlcpp_conversion.h>` (target
      `<lib>_cpp_hlcpp_conversion`). `fidl::HLCPPToNatural(...)` and
      `fidl::NaturalToHLCPP(...)` convert both domain types and protocol
      endpoints (`fidl::InterfaceHandle<P>` / `fidl::InterfaceRequest<P>` <->
      `fidl::ClientEnd<P>` / `fidl::ServerEnd<P>`). Migrating production code
      first while keeping existing HLCPP tests untouched preserves the tests as
      a behavioral oracle.

### 2. Update build targets

- Replace `<lib>_hlcpp` with `<lib>_cpp` in `BUILD.gn` (and `BUILD.bazel`; if
  `BUILD.gn` has a `BAZEL2GN SENTINEL`, edit `BUILD.bazel` and run `fx bazel2gn
  -d path/to/dir`).
- For test fakes inheriting from `fidl::testing::TestBase<P>`
  (`<fidl/fuchsia.foo/cpp/test_base.h>`), depend on `<lib>_cpp_testing`
  (`testonly = true`).
- Replace `//sdk/lib/sys/cpp` with `//sdk/lib/component/incoming/cpp` and/or
  `//sdk/lib/component/outgoing/cpp`.
- Drop `//src/lib/fostr/fidl/...` dependencies; every `<lib>_cpp` target already
  includes generated ostream formatters.

### 3. Migrate headers, types, clients, and servers

#### Includes, namespaces, and transitive headers

- Replace `#include <fuchsia/foo/bar/cpp/fidl.h>` (`fuchsia::foo::bar::Baz`)
  with `#include <fidl/fuchsia.foo.bar/cpp/fidl.h>` (`fuchsia_foo_bar::Baz`).
- Rename constants and enum members from `SCREAMING_SNAKE_CASE` to `kCamelCase`
  (for example, `MAX_ITEMS` -> `kMaxItems`, `Error::NOT_FOUND` ->
  `Error::kNotFound`), and replace `Protocol::Name_` with
  `fidl::DiscoverableProtocolName<fuchsia_foo::Protocol>`.
- **Add explicit `<lib/zx/*.h>` includes**: HLCPP headers accidentally pulled in
  Zircon handle headers transitively. Add `<lib/zx/vmo.h>`,
  `<lib/zx/eventpair.h>`, `<lib/zx/job.h>`, `<lib/zx/process.h>`,
  `<lib/zx/socket.h>`, etc. directly where used.

#### Domain types: tables, unions, handles, and copyability

- **Table initialization**: Natural C++ tables **do** support double-brace
  designated initializers, provided fields appear in **FIDL declaration/ordinal
  order** (otherwise `-Wreorder-init-list` fails to compile). Builder-style
  setter chaining also works when setting fields conditionally or out of order:

```cpp
auto params = fuchsia_diagnostics::StreamParameters{{
    .data_type = fuchsia_diagnostics::DataType::kInspect,
    .stream_mode = fuchsia_diagnostics::StreamMode::kSnapshot,
}};
// Or builder-style setters:
auto other = fuchsia_diagnostics::StreamParameters()
                 .stream_mode(fuchsia_diagnostics::StreamMode::kSnapshot)
                 .data_type(fuchsia_diagnostics::DataType::kInspect);
```

- **Table accessors**: `t.field()` returns `std::optional<T>&`. Always guard
  with `t.field().has_value()` or `t.field().value_or(...)`; calling
  `*t.field()` or `t.field().value()` when absent is UB/abort.
- **Unions and flexible types**: Construct unions via factory methods
  (`MyUnion::WithVariant(std::move(val))`) and inspect with `u.Which() ==
  MyUnion::Tag::kVariant`. Always add `default:` branches when switching on
  flexible enums or unions.
- **Optional handles**: `zx.Handle:<T, optional>` in FIDL maps to a plain
  `zx::T` in Natural C++ (where `!h.is_valid()` means absent), **not**
  `std::optional<zx::T>`.
- **Value vs resource copyability**:
  - Value structs, tables, and unions (containing no handles) are copyable.
    Audit `std::move(...)` sites inherited from HLCPP where moving a cached
    table or struct accidentally left the member empty on subsequent calls.
  - Resource types (containing handles) are move-only and have no `fidl::Clone`.
    Duplicate handles explicitly (`vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &copy)`).

#### Incoming service connections (preserve testability)

Do **not** replace `context->svc()->Connect<P>()` inside a class or library
directly with `component::Connect<P>()`. `component::Connect<P>()` hardwires the
process-global `/svc` namespace and makes unit testing impossible without
process-wide `fdio_ns_bind` hacks.

- **In classes and libraries**: Accept
  `fidl::UnownedClientEnd<fuchsia_io::Directory> svc` (or
  `fidl::ClientEnd<fuchsia_io::Directory>`) in the constructor or `Init` method
  and call `component::ConnectAt<P>(svc)`
  (`<lib/component/incoming/cpp/protocol.h>`).
- **In `main()` only**: Obtain `/svc` via `component::OpenServiceRoot()` or call
  `component::Connect<P>()` directly.

#### Clients, results, one-way calls, and events

- Bind `fidl::Client<P>` with an explicit synchronized `async_dispatcher_t*`, or
  use `fidl::SyncClient<P>` for blocking calls off the server thread.
- **Result storage**: `fidl::Result<M>` is neither copyable nor
  default-constructible. Take it by reference in callbacks (`[&](auto& result)`
  or `[&](fidl::Result<M>& result)`) and store the unwrapped payload
  (`result.value()`) as a member rather than storing `fidl::Result<M>`.
- **One-way `[[nodiscard]]` call results**: One-way calls return
  `fit::result<fidl::OneWayError>`.
  - In **tests**, assert success: `ASSERT_TRUE(res.is_ok()) <<
    res.error_value();` or `EXPECT_TRUE(res.is_ok());`.
  - In **production** code talking to peers that may legitimately close,
    `ZX_ASSERT(res.is_ok())` turns a benign peer-closed race into a crash (HLCPP
    silently ignored `ZX_ERR_PEER_CLOSED` on one-way writes). Check `status() !=
    ZX_ERR_PEER_CLOSED` instead (note that `is_peer_closed()` is private on
    `fidl::OneWayStatus`):

```cpp
if (auto res = client->OnEvent(payload);
    res.is_error() && res.error_value().status() != ZX_ERR_PEER_CLOSED) {
  FX_LOGS(WARNING) << "OnEvent failed: " << res.error_value();
}
```

- **Events and error handlers**: Inherit from `fidl::AsyncEventHandler<P>`,
  override `void OnFoo(fidl::Event<P::OnFoo>& event)` and `void
  on_fidl_error(fidl::UnbindInfo error)`, and pass `&event_handler_` to
  `client.Bind(std::move(client_end), dispatcher, &event_handler_)`. Declare the
  handler member **before** `fidl::Client<P>` so it outlives the client.

#### Servers, ownership, and outgoing directory

Inherit from `fidl::Server<P>` and update method signatures to `void
Method(MethodRequest& request, MethodCompleter::Sync& completer) override` (omit
`MethodRequest&` when the FIDL request is empty). Reply with
`completer.Reply(...)` or `completer.Reply(fit::ok(...))` /
`completer.Reply(fit::error(...))`.

Choose the server binding pattern that matches connection ownership:

| HLCPP pattern | Natural C++ replacement |
|---|---|
| `fidl::Binding<P>` member (owner-managed) | `fidl::ServerBinding<P>` member (declare **last** so it unbinds before sibling fields die). Prefer over `fidl::BindServer` + `ServerBindingRef`, whose `Unbind()` is async. |
| `fidl::BindingSet<P>` (shared server) | `fidl::ServerBindingGroup<P>`, published via `outgoing.AddUnmanagedProtocol<P>(bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure))`. |
| `fidl::BindingSet<P, std::unique_ptr<P>>` (self-owned per connection) | `fidl::BindServer(dispatcher, std::move(server_end), std::make_unique<Impl>(...))` (binding owns the `unique_ptr` and destroys it after unbind). |

If a connection's close handler removes its owning wrapper from a map and risks
re-entering container/allocator state mid-callback, defer destruction with
`async::PostTask(dispatcher, ...)`.

## Natural support library equivalents

Verify these drop-in natural overloads before hand-rolling VMO or token helpers:

- **`//src/lib/fsl/vmo`**:
  - `"src/lib/fsl/vmo/sized_vmo.h"`:
    `fsl::SizedVmo::FromTransport(fuchsia_mem::Buffer, &out)` and
    `std::move(sized_vmo).ToNaturalTransport()`.
  - `"src/lib/fsl/vmo/strings.h"` and `"src/lib/fsl/vmo/vector.h"`:
    `fuchsia_mem::Buffer` overloads of `fsl::VmoFromString`,
    `fsl::StringFromVmo`, `fsl::VmoFromVector`, and `fsl::VectorFromVmo`.
- **`//sdk/lib/fuchsia-mem-ext`**:
  - `<lib/fuchsia-mem-ext/fuchsia-mem-ext.h>`:
    `fuchsia_mem_ext::ExtractData(fuchsia_mem::Data)` and
    `fuchsia_mem_ext::CreateNaturalWithData(...)`.
- **`//sdk/lib/media/audio/cpp` and `//sdk/lib/media/cpp:no_converters`**:
  - `<lib/media/audio/cpp/types.h>`: natural `fuchsia_media::AudioSampleFormat`
    and `fuchsia_media::AudioStreamType` overloads of `media::BytesPerSample`
    and `media::CreateAudioStreamType`.
  - Depend on `//sdk/lib/media/cpp:no_converters` instead of
    `//sdk/lib/media/cpp` when HLCPP `TimelineFunction` `TypeConverter` is not
    needed.
- **`//sdk/lib/ui/scenic/cpp`**:
  - `<lib/ui/scenic/cpp/view_ref_pair.h>`: `scenic::cpp::ViewRefPair::New()` and
    `scenic::cpp::CloneViewRef(view_ref)`.
  - `<lib/ui/scenic/cpp/view_creation_tokens.h>`:
    `scenic::cpp::ViewCreationTokenPair::New()`.
  - `<lib/ui/scenic/cpp/view_identity.h>`:
    `scenic::cpp::NewViewIdentityOnCreation()`.
  - `<lib/ui/scenic/cpp/buffer_collection_import_export_tokens.h>`:
    `allocation::cpp::BufferCollectionImportExportTokens::New()`.
- **Stream formatting (`fostr` replacement)**:
  - `#include <fidl/fuchsia.foo/cpp/natural_ostream.h>` (built into every
    `<lib>_cpp` target) provides `operator<<` for generated domain types and
    `fidl::ostream::Formatted(val)` (`<lib/fidl/cpp/natural_ostream.h>`) for
    vectors, optionals, and handles.

## Migrating tests

1.  **Test fakes (`*_TestBase`)**: Inherit from `fidl::testing::TestBase<P>`
    (`<fidl/fuchsia.foo/cpp/test_base.h>`, target `<lib>_cpp_testing`) and
    implement the two-argument fallback. Close the completer before `FAIL()` (or
    use `ADD_FAILURE()`) so two-way completers do not abort on destruction:

    ```cpp
    void NotImplemented_(const std::string& name,
                         fidl::CompleterBase& completer) override {
      completer.Close(ZX_ERR_NOT_SUPPORTED);
      FAIL() << "Unexpected call: " << name;
    }
    ```

2.  **Single-protocol unit tests**: Skip service directories and wire endpoints
    directly:

    ```cpp
    auto [client_end, server_end] = fidl::Endpoints<fuchsia_foo::Bar>::Create();
    bindings_.AddBinding(dispatcher(), std::move(server_end), &fake_bar_,
                         fidl::kIgnoreBindingClosure);
    ```

3.  **Multi-protocol `/svc` unit tests** (replacing
    `sys::testing::ComponentContextProvider` / `ServiceDirectoryProvider`):
    - Use `component::testing::ServiceDirectoryProvider` from
      `//sdk/lib/component/testing/cpp`
      (`<lib/component/testing/cpp/service_directory_provider.h>`, in review at
      https://fxrev.dev/1961717; if not yet on `main`, serve a `vfs::PseudoDir`
      containing `vfs::Service` natural handlers) and pass `client_end()` to the
      code under test.
    - Keep the `ComponentContextProvider` raw-channel bridge
      (`fidl::ServerEnd<P>(hlcpp_req.TakeChannel())`) **only** for legacy shared
      test fixtures whose code under test still takes `sys::ComponentContext*`
      and cannot be refactored in the same CL.

4.  **C++ `RealmBuilder` local components**: Do **not** manually bridge channels
    through `LocalComponentImpl`. Inherit from
    `component_testing::LocalCppComponent`
    (`<lib/sys/component/cpp/testing/realm_builder_types.h>`), publish via its
    natural `outgoing()`, connect with `realm.component().Connect<P>()`
    (`zx::result<fidl::ClientEnd<P>>`), and use `[&](auto result) { ... }` in
    `realm.Teardown(...)` if needed:

    ```cpp
    class LocalBar final : public component_testing::LocalCppComponent,
                           public fidl::Server<fuchsia_foo::Bar> {
     public:
      explicit LocalBar(async_dispatcher_t* dispatcher)
          : dispatcher_(dispatcher) {}
      void OnStart() override {
        ZX_ASSERT(outgoing()
                      ->AddUnmanagedProtocol<fuchsia_foo::Bar>(
                          bindings_.CreateHandler(
                              this, dispatcher_, fidl::kIgnoreBindingClosure))
                      .is_ok());
      }
      // fidl::Server<fuchsia_foo::Bar> methods...

     private:
      async_dispatcher_t* dispatcher_;
      fidl::ServerBindingGroup<fuchsia_foo::Bar> bindings_;
    };
    ```

5.  **Avoid `ASSERT_*` hangs inside `.Then(...)` lambdas**: `ASSERT_*` returns
    only from the callback lambda, skipping trailing `QuitLoop()` or
    `latch.CountDown()` calls and hanging the test. Prefer `EXPECT_*`, or arm a
    `fit::defer` guard before `ASSERT_*`.
6.  **Same-thread dispatch**: Use `fidl::Client<P>` + `RunLoopUntilIdle()`
    rather than `fidl::SyncClient<P>` when the server runs on the same test
    thread. Because unbind is asynchronous, run `RunLoopUntilIdle()` after
    `client = {};` before asserting server-side disconnect state.

## Semantic behavioral checklist

Wire encoding is identical between HLCPP and Natural C++, so regressions come
from lifecycle, threading, and error-delivery differences. Audit every port
against these rules (see
[semantic_checklist.md](references/semantic_checklist.md) for the full
behavioral-difference matrix and 20-item deep review rubric):

- [ ] **Explicit synchronized dispatcher**: Every `fidl::Client` and
  `fidl::ServerBinding` binds on an explicit synchronized `async_dispatcher_t*`.
  Natural bindings panic on cross-thread access instead of HLCPP's silent UB;
  never swap in `fidl::SharedClient` / `fidl::WireSharedClient` to silence
  thread checks without designing two-phase async teardown.
- [ ] **`.Then(...)` vs `.ThenExactlyOnce(...)` lifetime**: Member-client calls
  capturing `this` (or test locals tied to fixture lifetime) use `.Then(...)`,
  which silently cancels when the client is destroyed (matching HLCPP
  `InterfacePtr`). `.ThenExactlyOnce(...)` **always** runs on unbind/destroy
  with `fidl::Reason::kUnbind` (`ZX_ERR_CANCELED`), causing UAF if `this` is
  already gone.
- [ ] **Async error handlers and N+1 peer-closed callbacks**: `on_fidl_error`
  and `CloseHandler` always run asynchronously on the dispatcher (except during
  `loop.Shutdown()`), never re-entrantly inside a send call. On peer-closed,
  **both** pending `.Then(...)` callbacks **and** `on_fidl_error` fire - guard
  against double-cleanup or duplicate disconnect counts.
- [ ] **`Close(epitaph)` and `CloseAll` teardown**:
  `ServerBinding::Close(epitaph)` is asynchronous and **invokes** `CloseHandler`
  with `fidl::Reason::kClose` (HLCPP `Binding::Close` was synchronous and
  skipped its error handler). `ServerBindingGroup::CloseAll` tears down
  asynchronously and fires the empty-set handler afterward.
- [ ] **Abandoned `Completer::Async` and encode failures disconnect**: Dropping
  an unanswered `Completer::Async` (for example, overwriting a pending
  hanging-get waiter) closes the channel with `ZX_ERR_BAD_STATE`
  (`fidl::Reason::kAbandonedAsyncReply`), whereas HLCPP silently dropped the
  callback. Server reply encode failures (oversized vectors, wrong handle
  rights) also tear down the binding.
- [ ] **Upfront event handlers**: `fidl::AsyncEventHandler<P>` is passed at
  `Bind(...)` time (Natural C++ has no post-bind `events().OnFoo = ...` setter).
- [ ] **`Request&` lifetime with `ToAsync()`**: Server `Request&` borrows from
  the decode buffer for the synchronous handler frame only - move or copy needed
  fields before calling `completer.ToAsync()`.
- [ ] **Safe optional and flexible-type access**: Table fields are checked with
  `.has_value()` or `.value_or(...)` before dereferencing, and `switch`
  statements on flexible enums/unions have `default:` cases.

## Verification and cleanup

1.  **Build and test**: Ensure modified targets are in the build graph (`fx
    add-test //path/to:target`; expect manual tests outside default CQ to need
    minor bit-rot fixes). Run `fx build` and `fx test`, plus repeat runs (`fx
    test --count 20 <test>`) to flush out async teardown races.
2.  **Format**: Run `fx format-code`.
3.  **Shrink HLCPP visibility and `enable_hlcpp`**:
    - Remove migrated entries from `build/cpp/hlcpp_visibility.gni` (and
      `vendor/google/build/hlcpp_visibility.gni` when applicable).
    - Check whether any FIDL library used by the target now has zero remaining
      `<lib>_hlcpp` or `<lib>_cpp_hlcpp_conversion` dependents (`git grep
      "<lib>_hlcpp"` across the repo and `vendor/`). If none remain, remove
      `enable_hlcpp = True` from its `BUILD.bazel` (and run `fx bazel2gn -d
      ...`) or `enable_hlcpp = true` from its `BUILD.gn`.
