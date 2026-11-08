---
name: kernel-review
description: Perform evaluation of a CL prior to a human reviewing.
---

# Presubmit Review

## Goal

Your goal is to use the following set of steps to evaluate the current git
commit and determine if it is ready for humans to spend time reviewing.

* DO NOT perform edits to the git commit (you may temporarily change the HEAD
  and working tree during investigations, you must put everything back).
* DO NOT upload to or manipulate gerrit in any way unless instructed.

## Process

Iterate through each of the numbered evaluation steps. A step may have a
condition on whether it should be applied or not. If applied, the evaluation
step might produce a report and/or a result. These should be collected and then
handled as per the final Report step at the end. Spawn subagents as applicable
to both parallelize these steps and prevent overflowing the context window.

### Subagent Model Selection & Orchestration

To minimize `pro` model calls without impacting review quality, you are
explicitly instructed to set the `Model` argument in `invoke_subagent` according
to the task type (this serves as explicit authorization to override the default
`"inherit"` model setting):

* **Use `Model: "flash"`** for mechanical, procedural, and tool-heavy steps:
    * **Step 0**: Pre-flight Commit Triage
    * **Step 1**: Commit Message evaluation
    * **Step 2**: Build and Test execution (escalate to `pro` only to diagnose
      complex build/test failures)
    * **Step 5 (Phase A)**: Disassembly extraction (`llvm-objdump` at `HEAD` and
      `HEAD~1` baseline)
    * **Step 9**: Code Formatting checks
* **Use `Model: "inherit"` (or `"pro"`)** for deep semantic, safety, and
  assembly reasoning:
    * **Step 3**: C++ -> Rust conversion (`babeldiff`)
    * **Step 4**: Fuchsia Review (`review`)
    * **Step 5 (Phase B)**: Performance and assembly/stack analysis
    * **Step 6**: Concurrency analysis
    * **Step 7**: Provenance analysis
    * **Step 8**: Unsafe Rust review
    * **Final Report**: Overall synthesis and readiness verdict

**Context & Artifact Efficiency Rules:**

1.  **Pass Step 0 context to downstream subagents**: Include the commit message,
    changed file paths, and relevant line ranges from Step 0 in every subagent's
    prompt so `pro` subagents do not spend turns running `git status` or `git
    log` to discover the commit contents.
2.  **Write artifacts directly from subagents**: Instruct each subagent that
    produces a report to write its report artifact directly to `<Artifact
    Directory Path>/` (with `UserFacing: true`) and return a concise summary
    plus the artifact path, so the main `pro` agent does not spend extra turns
    re-writing each subagent's report.
3.  **Avoid working-tree / build-directory races**: Any step that mutates the
    build directory or working tree (Step 2 builds, Step 5 `HEAD~1`
    checkout/build, or Step 9 `fx format-code`) must not run concurrently with
    steps reading the working tree during a `HEAD~1` checkout.

### 0. Pre-flight Commit Triage (`Model: "flash"`)

Spawn a `flash` subagent (or run a single combined inspection if trivial) to
inspect `HEAD` (`git log -1`, `git show --stat`, and the diff) and return a
structured summary containing:

1.  The full commit message, subject `[tag]`, `Bug:` footer(s), and `Test:`
    footer(s).
2.  The list of modified files and changed line ranges.
3.  Required build/test architectures for Step 2 (`x86` always, plus any
    arch-specific directories modified, plus any user-requested architectures)
    and whether `//zircon/kernel` is modified.
4.  Applicability flags for conditional steps:
    * **Step 3 (babeldiff)**: Does the commit implement existing C++ logic in
      Rust? (If yes, list the corresponding C++ and Rust file paths.)
    * **Step 6 (Concurrency)**: Does the diff or affected code involve locks,
      atomics, `UnsafeCell`, `spin_loop` / CPU yield hints, parallelism, or data
      structures shared concurrently across CPUs?
    * **Step 7 (Provenance)**: Does the diff manipulate raw pointers, store
      tagged pointers inside integer/atomic words, or notionally implement/run
      an allocator?
    * **Step 8 (Unsafe)**: Does the diff add, modify, or rely on `unsafe`
      blocks, functions, traits, inline `asm!`, `#[pin_data]` / `!Unpin` pinning
      guarantees, panic/halt handlers, or safe APIs in modules that uphold
      safety invariants?

### 1. Commit Message (`Model: "flash"`)

Delegate to a `flash` subagent to evaluate the commit message for suitability
against the following criteria:

1.  Has appropriate [tag] on the subject line.
2.  Has appropriate subject description.
3.  Contains a meaningful description, rationale and motivation of the contents
    of the commit.
4.  The commit does not contain changes that seem out of place, bundle unrelated
    subsystem edits, or mix secondary refactorings/optimizations into a port or
    delicate concurrency/lock-free change beyond what the commit message
    motivates.
5.  If the commit message has a Bug: footer check the bug and make sure that it
    exists and is a meaningful bug to use for this commit. It is fine for a Bug:
    footer to be absent.
6.  If the commit message has a Test: footer make sure the tests it references
    relevant and up to date for the current commit contents. It is fine for a
    Test: footer to be absent.

Commit messages tend to be fairly subjective. Your report should consider
absence of a [tag], a commit message that looks like a placeholder, or
substantive out-of-scope changes bundled into the commit (criterion 4) as
serious issues; other wording preferences should be minor recommendations.

### 2. Build and Test (`Model: "flash"` for execution; `pro` for complex failure diagnosis)

If the user requested to skip building and testing (e.g., "skip build", "no
build/test"), skip this step; otherwise perform it by default.

Delegate the build and test execution to a `flash` subagent:

First determine what architectures to build and test on. Use the following
criteria:

1.  x86 should always be built and tested.
2.  If the commit changes code inside arch specific directories or files then
    also include that architecture. Note that changing common code or files that
    are used by a particular architecture is insufficient to trigger this
    requirement.
3.  If the user specifically requested any additional architectures, include
    them.

Before performing code compilation first examine the commit and determine what
tests are related to the changes and whether the relevant tests / test packages
are included in the build.

Add any missing tests to the build configuration and then perform the build.

Run all the tests you previously determined as related to the change. If there
are ANY changes to the //zircon/kernel directory you must run the full
core-tests test suite.

Once building and testing in this step are complete (and if Step 5 is not
immediately reusing the live Bazel server), run `fx bazel shutdown` to release
open file descriptors held by the Bazel server.

**Failure handling:**

* Any build failures count as a reject for this change being ready to send for
  review.
* If the build and tests succeed (or fail due to an obvious syntax/import typo),
  the `flash` subagent prepares the Step 2 report artifact directly.
* If there are non-trivial build or test failures, the `flash` subagent must
  capture the exact compiler errors or failing test logs and return them so the
  `pro` agent can diagnose the root cause and prepare recommendations on how to
  fix them.

### 3. C++ -> Rust conversion (babeldiff) (`Model: "inherit"`)

If Step 0 determined that this change looks like a Rust implementation of
existing C++ code then perform this step, otherwise skip.

Spawn a `Model: "inherit"` subagent (passing the paired C++ and Rust file paths
from Step 0) to use the babeldiff skill to perform an analysis of the conversion
accuracy and evaluate the following C++-to-Rust porting checks, preparing a
report on its findings:

* **Manual Comment Checks Beyond `babeldiff`**: Because `babeldiff` primarily
  matches top-level function headers, manually verify:
    1.  **Struct Field & Enum Variant Comments**: All doc comments on C++ struct
        fields and enum variants are copied over to the Rust definitions. When
        two C++ types are merged into one Rust type or a struct is moved between
        headers, preserve the doc comments from all source declarations.
    2.  **Non-Trivial Inline "Why" Comments**: Important algorithmic or
        invariant comments inside C++ function bodies are preserved in the Rust
        port.
    3.  **Stale Cross-References & Positional Warnings**: Check for comments
        referencing deleted C++ headers (e.g., `"Must match foo.h"`) or relative
        positions in modified/pruned headers (e.g., `"See WARNING above"` when
        the referenced block was deleted or moved).
    4.  **Adapt Identifiers in Ported/Moved Comments**: Update C++ PascalCase
        method names (e.g., `Wait`, `Post`, `Block` -> `wait`, `post`, `block`),
        field/parameter names (e.g., `|foo_level|` -> `|foo_pages|`), and
        removed getter/setter references inside ported or moved comments to
        match the current Rust/C++ code.
    5.  **No References or Comparisons to Deleted C++**: Comments in ported Rust
        must state invariants in present terms rather than describing or
        comparing against C++ code deleted in the same CL (e.g., do not cite a
        `DEBUG_ASSERT` from a deleted `.cc` file).
    6.  **Facade Header Comments, Variant Accuracy, & `TODO:` Markers**:
        * New Rust facade methods for unported C++ functions must copy the C++
          `.h` declaration doc comments verbatim rather than inventing new
          summaries.
        * Verify that comments copied between `*_locked` and unlocked variants
          accurately reflect whether the lock was just released, and do not
          reference obsolete lock types (e.g., calling a mutex a spinlock).
        * Use a `TODO(https://fxbug.dev/...):` prefix (with a bug URL or owner
          to satisfy `clang-tidy`'s `google-readability-todo` check) on comments
          noting that a temporary FFI export, shim, manual struct mirror, or
          trait impl can be removed once remaining C++ types/callers are ported.
* **FFI Shim & Facade Ownership (Callee, Not Caller)**: Verify that `extern "C"`
  FFI shims (`cpp_*` / `rust_*`), `*_ffi.{cc,h,rs}` declarations, and opaque
  Rust facade types (`OpaqueFacade`) live in the **callee** module/subsystem
  that owns the underlying C++ API or type (e.g., KASAN helpers in
  `lib/instrumentation/asan`, arch quirks in `arch_rs`)—never in a caller's
  local `*_ffi.{cc,rs}` or an unrelated module, even if adding or extending a
  `-rs` target in the owning subsystem is needed. Before a CL introduces a new
  opaque facade, FFI wrapper, or bindgen-backed struct, search the tree to
  ensure a facade or Rust implementation does not already exist in another
  module; when an API is already implemented in Rust (including per-arch code
  under `#[cfg(target_arch = ...)]`), call or re-export the Rust function
  directly rather than routing Rust -> `extern "C"` -> Rust.
* **`*_ffi.rs` Boundary & Encapsulation**:
    1.  `*_ffi.rs` files must strictly contain `extern "C"` declarations and
        thin C++ <-> Rust proxy entry points (`rust_*` / `cpp_*`). New Rust
        domain types, helper functions, and inherent `impl` blocks belong in the
        primary module (`foo.rs`), not `foo_ffi.rs`.
    2.  Callers outside a type's defining module (or higher-level methods) must
        not call raw `bindings::cpp_*` / `ffi::cpp_*` functions directly on a
        Rust wrapper type; expose and call methods on the Rust type itself.
    3.  **Prefer References in `rust_*` FFI Trampolines & Preserve `RefPtr` /
        `const` Contracts**: When C++ passes a non-null pointer to an
        initialized `#[repr(C)]`, `Freeze` Rust type (e.g., `Deadline`), take
        `&T` / `&mut T` directly in the `extern "C" fn` signature (and `const
        T&` / `T&` in C++) rather than `*const T` / `*mut T` (or a `c_void`
        bindgen alias) with manual null checks and casts. When C++ forwards
        `const fbl::RefPtr<T>&` to a Rust FFI function taking `&T`: if `nullptr`
        was previously valid (e.g., returning `false`), preserve `ptr &&
        rust_fn(*ptr)` instead of unconditionally dereferencing `*ptr`; if
        `nullptr` is invalid, keep `DEBUG_ASSERT(ptr)` (or `&*ptr`) rather than
        `ptr.get()`, which drops `RefPtr::operator->()`'s debug null check. In
        C++ `extern "C"` declarations for logically mutating methods on opaque
        Rust types, take non-`const` `T*` (even if Rust takes `&T` via interior
        mutability) so non-`const` C++ wrapper methods do not trigger
        `clang-tidy` `readability-make-member-function-const`.
    4.  **Keep C++ Convenience Overload Delegation in C++**: When C++ overloads
        (e.g., `WaitQueue::Block`) simply forward to a single canonical C++
        method (`BlockEtc`), keep the overloads delegating in C++ and only route
        the canonical method across FFI (`rust_*`), avoiding extra FFI entry
        points and `TA_NO_THREAD_SAFETY_ANALYSIS` annotations in C++ headers.
    5.  **DRY FFI Struct Marshalling**: When multiple `rust_*` trampolines
        convert between a Rust struct and a `bindgen`/FFI struct, factor the
        conversion into `From<RustType> for bindings::FfiType` (or a helper)
        rather than duplicating field-by-field marshalling.
* **Global Variables Across FFI (`#[link_name]`)**:
    1.  When Rust needs to read or write a C++ global/static variable, prefer
        giving the C++ variable `extern "C"` linkage and binding it directly in
        Rust with `unsafe extern "C" { #[link_name = "..."] static ...; }`
        rather than writing trivial `cpp_get_*` / `cpp_set_*` C++ FFI accessor
        functions.
    2.  Never define a duplicate Rust `static` alongside an existing C++ global
        variable without linking them via `#[link_name = "..."]`—there must only
        ever be a single source of truth for global state during incremental
        porting.
* **Preserve Existing C++ Tests & Verify 1:1 Test Case Parity**:
    1.  Existing C++ unit test files (`*_tests.cc` / `UNITTEST(...)`) must
        **not** be deleted in the same CL that ports a component or adds Rust
        unit tests, unless there is a fundamental conflict where both cannot
        coexist. C++ tests validate the FFI shims/facades and behavioral parity
        and should remain in-tree until the C++ FFI facades are pruned in a
        follow-up CL.
    2.  **1:1 Test Case & Parameterized Variant Parity**: Enumerate every `TEST`
        / `UNITTEST` in the C++ test file—and every enum variant or mode in
        parameterized test helpers (e.g., `Signal::kKill` and
        `Signal::kSuspend`)—and verify each is ported to the Rust test suite
        whenever the underlying Rust APIs are available.
    3.  **No Redundant C++ Layout `static_assert`s in `*_tests.cc`**: When
        `zr::static_assert!` (`size_of`, `align_of`, `offset_of!`) in Rust
        already verifies layout against `bindgen`, do not move or duplicate
        C++-only `static_assert(offsetof(...))` checks into `*_tests.cc`.
* **1:1 Struct & State Fidelity**: Verify field-by-field parity between C++
  structs/globals and their Rust counterparts:
    1.  **Type Changes & Atomics Require an Actual Concurrent Race**: Plain C++
        fields or globals must not be silently changed to `Atomic*` (or
        duplicated in a parallel shadow wrapper type like `AtomicFoo` that can
        lose sync with `Foo`) when the C++ original used non-atomic
        reads/writes—especially when `#[unsafe(export_name = "...")]` shares the
        symbol with non-atomic C++ declarations. Changing a non-atomic type to
        `Atomic*` is allowed **only when there is an actual concurrent
        read/write access that would otherwise be a data race (UB) in Rust,
        accompanied by a comment justifying why `Atomic*` is required** (and
        provided it does not add unacceptable benchmark overhead). A comment
        merely explaining why `Ordering::Relaxed` is sufficient on a global
        written once during single-threaded boot before secondary CPUs start
        proves there is *no* concurrent race and does **not** justify `Atomic*`.
        When genuine C++ atomics are ported, prefer matching typed kernel
        atomics (`AtomicCpuMask`, `AtomicBool`, `AtomicPtr<T>`,
        `zr::AtomicConstPtr<T>`) over raw integer atomics.
    2.  **No Dropped Fields, Downgraded Types, or Hand-Rolled Intrusive Lists**:
        Flag any omitted C++ struct fields (even if unused by the functions in
        the current CL), helper types (e.g., `EventLimiter`) replaced with raw
        primitives, or C++ `fbl` intrusive containers (`DoublyLinkedList`,
        `SinglyLinkedList`, `WAVLTree`) replaced with hand-rolled raw-pointer
        linked lists when Rust `fbl` intrusive containers exist. Defer
        cross-object API/ownership restructures until the C++ counterparts are
        fully removed unless strictly required to avoid UB in Rust.
    3.  **Derived Default Values**: Ensure default values derived from C++
        struct initializers or configuration objects (e.g.,
        `BootOptions{}.pmm_checker_action`) are not replaced with hardcoded
        constants in Rust.
    4.  **Explicit `#[repr(...)]` & C++ `OpaqueStorage` Special-Member Safety**:
        Any Rust enum or struct whose layout or size is mirrored by C++
        `OpaqueStorage<SIZE, ALIGN>` must carry an explicit `#[repr(C)]`,
        `#[repr(u8)]`, `#[repr(C, u8)]`, or `#[repr(transparent)]` attribute
        (preferring `#[repr(transparent)]` on single-field newtype wrappers like
        `CriticalMutex` or `AutounsignalEvent` so call-ABI representation also
        matches) rather than relying on unspecified `repr(Rust)` layout, and
        must `zr::static_assert!` both `size_of` and `align_of` against C++. On
        the C++ side, replacing non-trivial fields (such as `fbl::RefPtr`) with
        `OpaqueStorage` inside a `public` struct gives it implicitly trivial
        default/copy/move constructors and destructor (risking uninitialized
        storage or shallow-copying an owned Rust `RefPtr`); hide `OpaqueStorage`
        privately or `= delete` copy/move operations and restrict construction.
    5.  **Pinning (`Pin`, `#[pin_data]`, `!Unpin`) Fidelity**:
        * Inside a `#[pin_data]` struct, never use an unannotated
          `PhantomData<PhantomPinned>` (which leaves the generated `Unpin` impl
          active) or a bare `#[pin] _pin: PhantomPinned` on types reachable from
          `extern "C"` signatures (which triggers `rustc`'s `improper_ctypes`
          lint); use `#[pin] _pin: PhantomData<PhantomPinned>` (or another
          FFI-safe `#[pin]` field that is `!Unpin`). Also verify that structs
          mirroring address-sensitive C++ `DISALLOW_COPY_ASSIGN_AND_MOVE` types
          whose fields are otherwise `Unpin` (e.g., `AtomicPtr<T>`) still opt
          into `#[pin_data]` + `!Unpin` and are marked `#[pin]` in parents.
        * When a `!Unpin` / `#[pin_data]` type is pin-initialized in place
          inside a `kalloc::Box` or `MaybeUninit` slot, retain
          `Pin<kalloc::Box<T>>` and use public `pin_init` APIs
          (`Box::try_pin_init(...)`, `MaybeUninit::write_pin_init(...)`) rather
          than stripping `Pin` into a plain `kalloc::Box<T>` or calling
          `#[doc(hidden)]` `__pinned_init` manually.
        * When interior mutability (`OpaqueBytes`, `UnsafeCell`, atomics) allows
          relaxing a method on an address-sensitive or intrusive type from
          `Pin<&mut Self>` to a shared reference, keep the receiver as `self:
          Pin<&Self>` rather than `&self`.
    6.  **Strict C++ `private:` Visibility Parity**: C++ `private:` methods and
        constructors whose only Rust callers are `rust_*` FFI trampolines in the
        same `.rs` file must remain private (`fn`), not `pub(crate)` or `pub`.
* **C++ Inheritance & Class Naming**:
    1.  When porting a C++ derived class (e.g., `Derived : public Base`), do not
        flatten/inline the shared base class into the derived struct; convert
        the C++ inheritance style into a Rust trait and/or composed member to
        preserve the original intent.
    2.  When a C++ class delegates its implementation over FFI to Rust, keep the
        original C++ class name (e.g., `class RunQueue`) rather than renaming it
        to `RustFoo` (`RustRunQueue`), which implies two separate types.
    3.  Conversely, when a Rust shim/facade wraps an unported C++ concrete class
        (e.g., `VmSlotPageStorage`, `VmLz4Compressor`), name the Rust struct
        after the C++ class directly rather than `CppFoo` plus a `pub type Foo =
        CppFoo` alias.
* **No Manual Duplication of SDK/Shared Definitions & Proper `bindgen` Usage**:
    1.  Do not manually redeclare constants, bitflags, or structs from
        `sdk/lib/...` or `zircon/system/public/...` (e.g., `zbi-format`) inside
        kernel `*_ffi.rs` files; use existing crates in `sdk/lib/rust` or
        generate bindings in the owning library.
    2.  When `bindgen` is used, ensure it follows standard Fuchsia bindgen
        workflows rather than ad-hoc checked-in copies paired with
        `LINT.IfChange` / `LINT.ThenChange` comments against C headers.
    3.  Do not place Rust constant or type definitions in `raw_lines` inside
        `kernel_bindgen_crate` (`BUILD.gn`); define them in a `.rs` module file.
    4.  Define object sizes and offsets in `object-constants.h` symbolically
        relative to base constants (e.g., `kBaseOffset + kBaseSize`) rather than
        hardcoded magic numbers.
* **Algorithmic Complexity Parity & Idiomatic Rust Signatures**:
    1.  Verify that container and data-structure operations (such as intrusive
        list `splice`, `size`/`len`, or tree traversals) do not degrade an
        $O(1)$ C++ operation into an $O(N)$ traversal in Rust.
    2.  When a C++ pointer parameter cannot be `nullptr` (unconditionally
        dereferenced or `DEBUG_ASSERT`ed non-null), the Rust signature should
        take `&T` / `&mut T` rather than `Option<&T>`.
    3.  Flag newly added methods on a type that duplicate the exact type
        signature and semantics of an existing accessor on that type (including
        methods already exposed via `Deref<Target = Base>`).
    4.  Prefer existing in-tree utilities (`zr::slice_from_raw_parts` /
        `zr::slice_from_raw_parts_mut` for C/FFI `(ptr, len)` pairs where `ptr`
        may be null when `len == 0`, `Box::try_pin_init`, `write_pin_init`,
        `pin_init::pin_init_array_from_fn`, `fbl::Vector::truncate`,
        `fbl::StringBuffer`, `&CStr` with `{:s}` in `kprint!`,
        `DurationMono::from_seconds(1).into_nanos()`, and
        `UnsafeCell::get_mut()` when holding `Pin<&mut Self>`) over
        `core::slice::from_raw_parts[_mut]` on potentially null empty pointers,
        hand-rolled `unsafe` closures, custom buffer types, magic numbers
        (`1_000_000_000`), or raw pointer dereferences in `Drop`.
    5.  **Uninitialized Out-Buffers & Signed C Return Casts**: Take `&mut
        [MaybeUninit<u8>]` rather than `&mut [u8]` for destination/output byte
        buffers (e.g., compression, decompression, or VMO/crashlog reads) so
        callers do not have to zero-initialize buffers up front. Never cast
        signed C/FFI return values (`c_int` / `ssize_t`, e.g., `FILE::write`)
        directly `as usize` without clamping or checking `< 0` (e.g., `max(res,
        0) as usize`), which sign-extends negative errors into huge unsigned
        counts.
* **Behavioral, Debug-Switch, Branch-Hint, & Control-Flow Parity**:
    1.  **`unlikely(...)` Branch Hints**: Preserve C++ `unlikely(...)` branch
        hints using `core::hint::cold_path()` in Rust.
    2.  **`ASSERT` vs. `DEBUG_ASSERT` & Fail-Fast Parity**: Flag unintended
        upgrades of `DEBUG_ASSERT` to `ASSERT` or `debug_assert!` to `assert!`
        (especially when `assert!` calls an FFI function like `ints_disabled()`
        in a hot path), flag replacing a C++ release-build silent return on null
        (`DEBUG_ASSERT(p); if (!p) return;`) with an unconditional `.unwrap()`,
        and conversely flag unintended *downgrades* of C++ release-build panics
        to `.ok()` + `debug_assert!` or moving top-of-function single-init
        `ASSERT(!instance_)` checks after side-effecting mutations. When a safe
        Rust function adds a non-debug `assert!` or saturating `c_int::try_from`
        check absent from C++ because safe callers could pass arbitrary slice
        lengths into an `unsafe` C call, require a comment explaining why
        soundness requires it.
    3.  **Compile-Time Debug & Feature Gates (`LK_DEBUGLEVEL`,
        `ZX_DEBUG_ASSERT_IMPLEMENTED`, Sanitizers)**: Do not hardcode C++
        `LK_DEBUGLEVEL` checks (e.g., `WAIT_QUEUE_VALIDATION`) to `false` in
        Rust; wire them to `cfg!(...)`. Verify `#[cfg(...)]` and `BUILD.gn`
        gates (such as KASAN/sanitizer or `kernel_no_userabi` configs) match C++
        build conditions rather than compiling out needed modules. In C++
        headers using `<zircon/assert.h>`, use `#if ZX_DEBUG_ASSERT_IMPLEMENTED`
        consistently.
    4.  **Control-Flow, Panic Formatting, & Helper Parity**: Keep function
        boundaries and control flow aligned with C++ (avoid inventing extra
        helper functions like `flush_range` or `#[cold] fn unexpected_*` when
        inline code or `panic!` suffices, and flag edits to existing functions
        outside the scope of the port). Use existing string-formatting helpers
        (e.g., `page_state_to_string(state)`) instead of `{:?}` (`Debug`) when
        matching C++ panic/log messages, and omit trailing `\n` in Rust `panic!`
        messages.
* **Orphaned Cross-Language FFI, C++ Header, `BUILD.gn`, & Doc Dead-Code
  Audit**: Audit both the ported module and any collaborating modules for code
  or doc links made dead by the port:
    1.  **Dead C++ Methods, FFI Shims, & `#![allow(dead_code)]` Helpers**:
        Remove C++ declarations, `.cc` forwarding shims, `rust_*` / `cpp_*` FFI
        entry points, and unused Rust methods (including dead helpers masked by
        crate-level `#![allow(dead_code)]`) that have no remaining callers once
        callers move to Rust, and tighten the corresponding Rust functions'
        visibility (`fn` or `pub(super)`).
    2.  **Orphaned FFI-Only Parameters & Conversion Helpers**: Remove Rust
        helpers or parameters that only existed to serve removed C++ FFI callers
        (e.g., C++-supplied `now` timestamps on `*_with_ticks` wrappers, unused
        `as_raw` / `from_raw` methods, or unused parameters on manual
        `rust_*_init` functions).
    3.  **Layout-Only C++ Types & `OpaqueStorage`**: When C++ no longer
        constructs, embeds, or takes `sizeof(T)` of a ported type (only passing
        `T&` / `T*`) or no longer uses an auxiliary struct/enum across FFI
        (`Stats`, `State`, `StateLayout`, `OpaqueStorage` fields), remove the
        C++ definitions, `bindgen` allowlist entries, and `zr::static_assert!`
        checks that kept them alive solely for layout assertions.
    4.  **Empty `*_ffi.h` Headers, Unused `#include`s/`deps`, & Stale `bindgen`
        Rules**: Delete emptied `*_ffi.h` headers, remove unused `#include`s and
        forward declarations from modified `.h`/`.cc` files, prune unused C++
        `deps` and stale single-file `source_set`s in `BUILD.gn` once `.cc`
        includes are removed, and omit disallowed `output_name`/`edition` args,
        redundant `function_allowlist = []`, or unreachable `type_blocklist`
        entries (under `--no-recursive-allowlist` + `opaque_types`) in
        `kernel_bindgen_crate` (`BUILD.gn`).
    5.  **Documentation Link Hygiene (`doc_checker`)**: When `.cc`/`.h`/`.rs`
        files are deleted, moved, or ported, search `docs/` and in-tree Markdown
        files for links to the deleted/moved file paths or line anchors (e.g.,
        `/zircon/kernel/.../foo.cc#28`) and update the surrounding documentation
        prose so `doc_checker` does not fail in CQ.

### 4. Fuchsia Review (`Model: "inherit"`)

Spawn a `Model: "inherit"` subagent (passing the changed file list from Step 0)
to use the standard 'review' skill to perform a code review and forward the
report on its findings. In addition to the standard review checks, ensure the
subagent evaluates:

* **Bug-Fix Regression Tests**: If the commit is a bug fix or behavioral change
  (rather than a pure refactoring/porting change) and does not include a new or
  updated test, check whether a userspace core test
  (`//zircon/system/utest/core/...`) or kernel unit test (`#[unittest::test]`)
  can be written to demonstrate the fix or behavior change.
* **Cold-Path Hint Placement Analysis (`core::hint::cold_path()`)**:
  Independently of C++ parity, analyze conditional branches across added or
  modified Rust functions to identify rare, slow-path, fallback, error/abort,
  contention, or diagnostic/logging branches inside hot or inline functions
  where calling `core::hint::cold_path()` would improve branch layout and keep
  fast-path code generation compact.
* **Hard Ban on `core::hint::spin_loop()`**: Treat any use of
  `core::hint::spin_loop()` or `std::hint::spin_loop()` in kernel or shared
  Zircon Rust code as a blocking issue (it is broken on arm64 emulators);
  require `arch_rs::r#yield()` in kernel code (`libarch::r#yield()` in
  host/shared builds).
* **Scope Discipline & Avoiding Transitional Churn**: Flag unrelated subsystem
  changes or secondary optimizations bundled into a port or delicate
  lock-free/concurrency CL so they can be split into separate CLs, and question
  churning transitional single-TU C++ headers or FFI shims that will soon be
  deleted altogether when ongoing porting finishes.
* **Cross-Cutting FFI, API, and Comment Hygiene** (especially when Step 3 was
  skipped, e.g. on FFI-pruning or standalone Rust/C++ CLs):
    * FFI shims/facades live in the owning callee subsystem crate (never a
      caller module), already-ported Rust functions are called directly rather
      than via `extern "C"`, `*_ffi.rs` files only proxy C++ <-> Rust without
      defining domain types/methods, and callers invoke methods on Rust types
      rather than raw `bindings::cpp_*` calls.
    * Public C++ `OpaqueStorage` wrappers do not expose trivial copy/uninit
      constructors over owned Rust state (`RefPtr`), `fbl::RefPtr` -> `&T` FFI
      forwarding preserves null handling or `DEBUG_ASSERT(ptr)`, and mutating
      C++ `extern "C"` declarations take non-`const` `T*`.
    * No orphaned C++ methods, `rust_*`/`cpp_*` FFI shims, `as_raw`/`from_raw`
      helpers, `#![allow(dead_code)]`-masked dead methods, layout-only C++
      structs/enums (`OpaqueStorage`/`StateLayout` when C++ only uses
      `T&`/`T*`), empty `*_ffi.h` headers, unused `#include`s/forward
      declarations, unused C++ `BUILD.gn` `deps`/stale `source_set`s, disallowed
      `kernel_bindgen_crate` args (`output_name`, `edition`) or redundant
      allowlists/blocklists, or broken `docs/` links (`doc_checker`) to deleted
      files.
    * No manual duplication of `sdk/lib/...` definitions, misuse of `bindgen`
      with `LINT.IfChange`, or Rust definitions inside `bindgen` `raw_lines`.
    * Proper `Pin` / `#[pin_data]` hygiene (`#[pin] _pin:
      PhantomData<PhantomPinned>` rather than unpinned
      `PhantomData<PhantomPinned>` or FFI-unsafe bare `PhantomPinned`, `!Unpin`
      on `AtomicPtr`-backed address-sensitive wrappers, `Pin<kalloc::Box<T>>`
      via `Box::try_pin_init` for heap-pinned `!Unpin` types, and `self:
      Pin<&Self>` on shared methods of pinned types).
    * Preserve non-obvious inline "why" comments from deleted `*_ffi.{cc,rs}`
      files when inlining calls into callers; check for broken relative comments
      (e.g., `"See WARNING above"` after header pruning), stale
      identifier/parameter names in moved comments, missing
      `TODO(https://fxbug.dev/...):` prefixes on temporary shims, degraded
      $O(N)$ container operations, `Option<&T>` parameters where `None` is
      invalid, duplicate accessors (including `Deref` duplicates), or verbose
      repeated module paths where a `use` import suffices.

### 5. Performance and Code Generation (`Model: "flash"` for disassembly collection -> `Model: "inherit"` for analysis)

Determine the requested performance analysis level (`none`, `brief`, or
`thorough`):

* **None** (default if not requested): Skip this step.
* **Brief** (when the user requests a "brief" performance analysis, or requests
  a performance analysis without specifying "thorough"):
    * **Reuse the existing build configuration** (from Step 2, or the current
      build directory if Step 2 was skipped). Do **not** run `fx set`, change
      build arguments, or compile additional architectures.
    * **At most one additional incremental build (baseline without the CL):**
        1.  First, using the output from Step 2 (`HEAD`)—or building only the
            specific affected target(s) at `HEAD` if Step 2 was
            skipped—disassemble **only** the specific modified or added
            functions (`llvm-objdump --disassemble-symbols=<sym>`).
        2.  If a before/after comparison is applicable (e.g. modifying existing
            functions or converting C++ -> Rust), you may temporarily checkout
            `HEAD~1`, perform **one** incremental build of only the specific
            affected target(s), extract the disassembly for the baseline
            symbols, and immediately restore `HEAD`. (Ensure no other steps or
            subagents are concurrently reading the working tree while `HEAD~1`
            is checked out.)
    * **No experimental edits or rebuilds:** Do **not** modify code or data
      structures to test potential optimizations; recommend improvements based
      on your analysis of the source and disassembly.
* **Thorough** (only when the user explicitly requests a "thorough" performance
  analysis):
    * Analyze both x86 and arm objdumps for this change (building both
      architectures if not already built).
    * If the change is meant to be behavior-preserving (for example if it's a
      C++ -> Rust conversion) then evaluate for performance parity against the
      baseline.
    * If introducing new logic and behaviors, check if the code generation is
      obviously 'bad' and if there are minor code / data changes that could
      improve it, testing candidate changes if needed.

**Kernel-Only Builds for Disassembly:**

When disassembling `//zircon/kernel` code (at `HEAD` if Step 2 was skipped, at
`HEAD~1` for a baseline comparison, or across architectures / candidate changes
in `thorough` mode), you **only** need to build the kernel ELF binary rather
than running a full `fx build` (which unnecessarily compiles userspace packages,
ZBIs, and Bazel product bundles):

```bash
fx [--dir=<build_dir>] build kernel_<arch>.<switchset>[-<variant>]/<kernel_target>
```

For example, `fx --dir=out/arm build
kernel_arm64.lk_debug_level_2/vmzircon.with-tests` or `fx build
kernel_x64.lk_debug_level_0/vmzircon`. Choose each component to match the build
directory and analysis goal:

* **Build directory (`--dir=<build_dir>`)**: Omit to use the current default
  `fx` build directory, or pass `--dir=<build_dir>` when targeting a specific or
  per-architecture build directory (e.g., `out/arm`, `out/x64`).
* **Architecture (`<arch>`)**: `x64`, `arm64`, or `riscv64`, matching the target
  architecture configured in `<build_dir>`.
* **Debug level / switchset (`<switchset>`)**: Reuse the `kernel_*` toolchain
  directory already populated in `<build_dir>` (when Step 2 has run) and keep it
  identical between `HEAD` and `HEAD~1`. Common switchsets are
  `lk_debug_level_2` (`LK_DEBUGLEVEL=2`, debug assertions enabled, standard for
  `eng`) and `lk_debug_level_0` (`LK_DEBUGLEVEL=0`, debug assertions disabled as
  in `user` / `userdebug`, useful when `DEBUG_ASSERT` / `debug_assert!` checks
  obscure hot-path codegen), plus any active toolchain variant suffix (e.g.,
  `-kasan`). Never use `static-checks` (`kernel_<arch>.static-checks`), which
  compiles unoptimized (`optimization_mode = "none"`) for static lints only.
* **Kernel binary (`<kernel_target>`)**: Use `vmzircon.with-tests` when unit
  tests are enabled in the switchset (such as `lk_debug_level_2`, matching `eng`
  builds and required when disassembling test symbols), or `vmzircon` for a
  standalone kernel without built-in unit tests (faster to link when test
  symbols are not needed, and required when unit tests are disabled such as in
  `lk_debug_level_0`). The build writes the unstripped ELF directly to that same
  `<build_dir>/kernel_<arch>.<switchset>[-<variant>]/<kernel_target>` path for
  `llvm-objdump`.

**Two-Phase Execution for Step 5:**

1.  **Phase A — Disassembly Collection (`Model: "flash"`)**: Use a `flash`
    subagent (run after Step 2 completes) to locate (or minimally build, using
    the kernel-only target above for `//zircon/kernel` changes) the compiled
    binary/object files, resolve mangled symbol names for the modified/added
    functions, run `llvm-objdump --disassemble-symbols=<sym>` at `HEAD` (and
    `HEAD~1` if applicable, restoring `HEAD` immediately), run `fx bazel
    shutdown` when all builds are finished, and write the extracted disassembly
    to scratch files in `<Artifact Directory Path>/scratch/`.
2.  **Phase B — Assembly & Stack Analysis (`Model: "inherit"`)**: Using the
    `pro` model (either directly in the orchestrator or via a `Model: "inherit"`
    subagent in a single pass over the scratch disassembly files and source
    diff), evaluate the disassembly. Be especially mindful of unnecessary or
    excessive stack allocations / usage in kernel code, and check whether
    rare/fallback/diagnostic branches inlined into hot paths need
    `core::hint::cold_path()` to move cold code out of the hot instruction
    stream. Prepare a report on any issues and potential improvements that could
    be made.

### 6. Concurrency (`Model: "inherit"`)

If Step 0 determined that the code change or its surrounding context involves
concurrency, parallelism, locks, atomics, `UnsafeCell`, or data structures
shared across CPUs, perform this step; otherwise skip.

Spawn a `Model: "inherit"` subagent to examine the code change with an eye for
concurrency and parallelism. Items you are specifically evaluating:

* Use of locking is appropriate and minimal.
* Lock scopes are not widened across operations (such as `thread.resume()`) that
  C++ performed outside the lock, atomic loads are not hoisted out of
  conditional branches, and redundant lock assertions (e.g.,
  `assert_lock_not_held` immediately after `assert_no_transaction_active`) are
  avoided.
* Runtime trace arguments (such as `PerCpu` or active transaction reads for
  `ktrace`) are evaluated lazily inside the trace macro rather than eagerly
  before checking whether the trace category is enabled.
* Never use `core::hint::spin_loop()` or `std::hint::spin_loop()` in spin-wait
  loops (broken on arm64 emulators); require `arch_rs::r#yield()` /
  `libarch::r#yield()`.
* 'manual' concurrency using `UnsafeCell` looks correct—in particular, when a
  `// SAFETY:` comment claims a global or field is written only during
  single-threaded early boot before secondary CPUs start, verify in the boot
  init call order that the write actually precedes `platform_init_smp()` / AP
  bringup (e.g., `x86_bringup_aps`).
* **Zero `Freeze` Bytes on Concurrent Self-Destructing Types & No Live `&Self`
  Across Blocking/Unlock**: No shared reference (`&Self`) or lifetime-unified
  lock guard remains in scope across a thread-blocking call (e.g.,
  `scheduler::block` / `block_etc_postamble`) or lock release after which
  another CPU/thread may destroy the object. Furthermore, on synchronization
  primitives (`Mutex`, `Event`, `Semaphore`, `WaitQueue`) where a non-blocking
  method taking `&self` (e.g., `release(&self)`, `signal(&self)`) performs an
  atomic state transition after which another CPU can immediately acquire,
  release, and destroy the object (`~T` / `Drop`, such as zeroing `fbl::Canary`
  magic) *before* `&self` returns, Stacked Borrows and Tree Borrows function
  protectors (`LLVM readonly`) make any bare `Freeze` field in `Self` immediate
  UB: verify **every** field mutated during init or destruction (`Canary` magic,
  lock-name/spin-tracing storage) is wrapped in `UnsafeCell` (zero `Freeze`
  bytes), and verify `PinnedDrop::drop(self: Pin<&mut Self>)` does not violate
  `# Safety` aliasing claims while background threads still hold `&Self`.
* Data structures that might be used concurrently across CPUs are structured to
  minimize false contention.
* Memory orders on atomic operations. If converting existing C++ memory orders
  should exactly match the C++. Any memory order that is not Sequentially
  Consistent should have a justification as to why this is not the case (note
  that single-init `swap(true, Ordering::Relaxed)` guards with no paired
  `Release` store should use `Relaxed` rather than `Acquire`).

Prepare a report on your findings.

### 7. Provenance (`Model: "inherit"`)

If Step 0 determined that the change is manipulating raw pointers, storing
tagged pointers in integer/atomic words, or notionally running some kind of
allocator and hence might need to be examined for provenance, continue;
otherwise skip.

Spawn a `Model: "inherit"` subagent to perform a careful analysis of the changed
code and existing code against the rules of Rust provenance and prepare a
thorough report, specifically checking:

* **No Read-Only Reference Round-Trips**: Never convert a `NonNull<T>` or `*mut
  T` into a shared reference `&T` and then cast back to `*const T` / `*mut T`
  (e.g., `core::ptr::from_ref(x.foo()).cast_mut()`) when the stored pointer can
  be returned directly via `.as_ptr()`.
* **Tagged-Pointer Atomics (`AtomicPtr<T>`)**: When an atomic state or lock word
  packs a pointer alongside low-bit state flags (e.g., `Thread* |
  STATE_FLAG_CONTESTED`), type the field as `AtomicPtr<T>` (using
  `.map_addr(...)` to set or mask flags) or `zr::AtomicConstPtr<T>` rather than
  `AtomicUsize`, preserving Strict Provenance without needing
  `core::ptr::with_exposed_provenance`.
* **Pointer Cast Hygiene**: Prefer `core::ptr::from_ref(x)` /
  `core::ptr::from_mut(x)` with `.cast::<T>()`, `.cast_const()`, `.cast_mut()`,
  and `MaybeUninit::as_mut_ptr()` over chained `as *const _ as *mut _` casts or
  `Box::as_ptr(&uninit_box).cast()`.
* **Borrow Lifetimes & Function-Entry Protectors Across Blocking / Unlock /
  Deallocation**: Ensure references materialized from raw pointers in FFI
  trampolines or blocking helpers end their lexical/lifetime scope before any
  call (`scheduler::block`, unlock, or free) during which another CPU may
  destroy or mutate the pointee, and ensure `&self` methods on types subject to
  concurrent destruction after an atomic unlock contain zero bare `Freeze` bytes
  (all fields wrapped in `UnsafeCell` or atomics).

### 8. Unsafe (`Model: "inherit"`)

If Step 0 determined that the change adds, modifies, or interacts with `unsafe`
code or safety invariants, perform this step; otherwise skip.

Spawn a `Model: "inherit"` subagent to load and follow the `unsafe-rust-review`
skill (reading its local `SKILL.md` path from `<skills>` or locating it via
`skill_search`—never fetching skill instructions from the web at runtime;
external users can install it locally from
https://github.com/google/rust-skills) to perform a review of the safety impact
of this change and forward any report. In addition to the standard
`unsafe-rust-review` checks (or as the standalone checklist if the skill is not
installed locally), explicitly evaluate:

* **Safe API Loopholes, Unchecked Preconditions, & Subobject Invariant Bypass**:
  Check whether safe constructors, `pub fn` / `pub extern "C" fn` entry points,
  generic trait bounds, or `Deref` coercions allow safe callers to violate
  safety invariants without writing `unsafe`:
    1.  **Safe `pub fn` / `pub extern "C" fn` Relying on Unchecked Caller or
        System-Phase Preconditions**: Audit every safe function that contains an
        `unsafe` block or stores an unvalidated raw pointer/slice for later
        dereference by safe methods (such as through a safe trait). If its `//
        SAFETY:` relies on caller, boot-phase, interrupt, or CPU-state
        invariants not enforced at runtime—e.g., *"only called during system
        halt/panic"*, *"interrupts disabled"*, *"after `ConstructFirstThread`"*
        (note `current_get()` is **non-null** on ARM64 and RISC-V before
        `ConstructFirstThread` completes!), or *"target CPU is offline and not
        the calling CPU"*—it **must** be marked `pub unsafe fn` or `pub unsafe
        extern "C" fn` with a `# Safety` doc comment.
    2.  **Nullable C/FFI `(ptr, len)` Slice Construction**: Flag
        `core::slice::from_raw_parts` / `core::slice::from_raw_parts_mut` on
        C/FFI pointers unless `ptr` is guaranteed non-null and aligned even when
        `len == 0`; require `zr::slice_from_raw_parts[_mut]` when empty C
        buffers may pass `nullptr`.
    3.  Safe initializers accepting raw magic/discriminant values (e.g.,
        `init(magic: u32)`) that could trigger an unchecked C++ downcast
        (`static_cast<Derived*>`).
    4.  Safe methods on a base type reachable via `Deref` or `.as_base()` (e.g.,
        `WaitQueueBase::collection_mut`) that bypass an `unsafe` invariant on a
        derived type (`OwnedWaitQueue`), or outer `Deref` wrapper `as_mut_ptr`
        methods that return the wrapper's address instead of forwarding to the
        inner C++ lock/object when feature-gated prefix fields (`lock_dep`)
        shift the inner offset.
    5.  Safe token reborrow helpers returning a by-value token (`reborrow(&mut
        self) -> Token`) that can be passed to a lock-releasing (`TA_REL`)
        method without disarming the owning guard (causing a double-release on
        `Drop`).
    6.  **Generic `unsafe trait` Relaxation**: When relaxing an `unsafe trait`'s
        contract (e.g., widening a lock-policy trait from exclusive-only to
        shared-or-exclusive), audit every safe generic API and proc-macro
        bounded on that trait so shared-lock policies cannot obtain aliasing
        `&mut T` references without an `ExclusiveLockPolicy` subtrait bound, and
        ensure split hook sequences (`pre_validate` + `post_validate_acquire`)
        are documented in `# Safety`.
* **Inline `asm!` Options & Early-Boot / Panic Handler Fallibility**:
    * Audit `core::arch::asm!` options (`nostack`, `pure`, `readonly`,
      `preserves_flags`). Instructions that modify page tables / address spaces
      (`cr3`, `ttbr*`, `satp`) or stack pointers can switch or remap the active
      stack from the compiler's perspective and must **never** specify
      `options(nostack)` (nor `preserves_flags` when flags are clobbered).
    * In panic, crashlog, halt, and reboot paths (reachable before per-CPU,
      microarch, or SMP initialization completes), never `.unwrap()` optional
      early-boot state in a way that recursively panics instead of falling
      through to fallback reboot/halt paths, and ensure early-boot allocation
      failures leave global/SMP state consistent for later init stages.
* **Propagating Subobject & Transitive C++ Preconditions into `# Safety`**:
  Ensure `pub unsafe fn`s on base types document conditional derived-type
  (`OwnedWaitQueue`) invariants in `# Safety` and at `// SAFETY:` call sites,
  and ensure FFI facade functions document transitive C++ scheduler and
  transaction preconditions (`THREAD_RUNNING`, blocking permitted,
  `AssertNumLocksHeld(1)`, `ChainLockTransaction` not finalized on entry and
  finalized on exit).
* **FFI Callback Trait Execution Context & Non-Aliasing**: When a safe Rust
  trait is invoked from C++ with interrupts disabled or locks/transactions held,
  require `# Execution Context / Panics` docs on the trait and a caller `#
  Safety` precondition that no conflicting proof/guard (`ChainLockProof` /
  `OwnerGuard`) is live across the call.
* **Pinning (`#[pin_data]`, `Pin<kalloc::Box<T>>`) & `PinnedDrop` Soundness**:
  Verify `#[pin_data]` structs use `#[pin] _pin: PhantomData<PhantomPinned>`
  (never unpinned `PhantomData<PhantomPinned>`, and never bare `PhantomPinned`
  on FFI-reachable types), `!Unpin` is preserved on `AtomicPtr`-only
  address-sensitive wrappers, heap-allocated `!Unpin` types retain
  `Pin<kalloc::Box<T>>`, and `PinnedDrop::drop(self: Pin<&mut Self>)` does not
  violate `# Safety` aliasing claims while background threads hold `&Self`.
* **`// SAFETY:` Placement, Accuracy, & Compile-Time Proofs**: Verify that `//
  SAFETY:` comments only appear immediately above an `unsafe` block or `unsafe
  impl` (never above safe assignments), that any code expressions cited in `//
  SAFETY:` match the actual code, and that compile-time layout/size invariants
  relied on by `// SAFETY:` (e.g., stack guard sizes matching a constant) are
  enforced via `zr::static_assert!`.

### 9. Code Formatting (`Model: "flash"`)

Delegate to a `flash` subagent:

Use the 'format-code' fx tool to check that the current commit has been
correctly formatted (ensure any working-tree changes made by `fx format-code`
are reverted after capturing the diff so the working tree remains clean). For
any `.rs`, `.cc`, and `.h` files in the commit, explicitly check all added lines
to make sure they are `<=100` characters in width—including comments (`//`,
`///`) and code/strings inside macro invocations (`dprintf!`, `kprintln!`,
`assert!`, etc.) that `fx format-code` (`rustfmt`) does not wrap.

For Rust in places where doc comments can go (above functions, structs, members,
etc) if there is a '//' comment determine if that is a mistake or should be a
doc comment. In `///` doc comments, check that a blank `///` line separates any
numbered or bulleted list from a subsequent paragraph.

Scan added identifiers (register/constant/type names), comments, strings, and
the commit message for transposed-letter typos (e.g., `"TRASNIENTCFG"` instead
of `"TRANSIENTCFG"`, `"readablity"` instead of `"readability"`) as well as
common Gerrit Spellchecker warnings (e.g., `"re-use"` instead of `"reuse"`,
`"defered"` instead of `"deferred"`).

Check added or modified C++ and Rust files for common `clang-tidy` and header
hygiene warnings: bare `TODO:` without a bug link or owner
(`TODO(https://fxbug.dev/...):` for `google-readability-todo`), mismatched
parameter names between `.h`/forward declarations and `.cc` definitions
(`readability-inconsistent-declaration-parameter-name`), missing direct
`#include`s in `.h` / `*_layout.h` headers (Include-What-You-Use), signed
integer operands in bitwise expressions (`bugprone-signed-bitwise`, e.g., use
unsigned `14u` literals), Cisms in `.cc` translation units (`#include
<stdbool.h>`, `(void)` parameter lists), and C++ methods modified to call
`const` FFI functions that can now be marked `const`
(`readability-make-member-function-const`).

Summarize any issues in a report.

## Report

Each of the evaluation steps, if they needed to be run, will have produced their
own report artifact (written directly by the subagent or by you). Examine all of
the step reports and provide a summarized version of any issues or actionable
resolutions along with an overall recommendation on whether this change is ready
for human review.
