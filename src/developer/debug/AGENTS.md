# `src/developer/debug` Development Guide for AI Agents

Follow these cross-cutting rules and progressive-disclosure gates when you
write, refactor, or review code under `//src/developer/debug`.

## Subsystem Map & Mandatory `README.md` Pre-Edit Gate

Each subsystem maintains a local `README.md` contract with `## Interface`, `##
Invariants`, `## Side Effects`, and `## Verification` sections.

**Mandatory Pre-Edit & Code Review Gate**: Before you edit or review (`/review`)
any file in a subdirectory, read `## Invariants` and `## Verification` in its
`README.md`.

Each subsystem maps to a local contract, build target, test suite, and
architectural layer:

- **`debug_agent/`** ([README.md](./debug_agent/README.md),
  [posix](./debug_agent/posix/README.md)): Target-side Fuchsia and Linux stub
  (`DebugAgent`, `DebuggedJob`, `DebuggedProcess`, `DebuggedThread`,
  `LimboProvider`). Build `//src/developer/debug/debug_agent` and run `fx test
  debug_agent_unit_tests`.
- **`ipc/`** ([README.md](./ipc/README.md)): Versioned binary wire protocol
  (`debug_ipc`) between `zxdb` and `debug_agent`. Build
  `//src/developer/debug/ipc` and run `fx test zxdb_tests`.
- **`shared/`** ([README.md](./shared/README.md),
  [logging](./shared/logging/README.md)): Cross-platform `MessageLoop`,
  `StreamBuffer`, `BufferedStream`, `Arch`, `RegisterID`, `Status`, and
  `Result`. Build `//src/developer/debug/shared` and run `fx test zxdb_tests`.
- **`zxdb/common/`** ([README.md](./zxdb/common/README.md)): **Layer 1
  (Lowest)** base utilities (`Err`, `ErrOr`, `AddressRanges`). Must not depend
  on other `zxdb` layers. Build `//src/developer/debug/zxdb/common` and run `fx
  test zxdb_tests`.
- **`zxdb/symbols/`** ([README.md](./zxdb/symbols/README.md)): **Layer 2** DWARF
  and ELF symbol indexing (`Index`, `DwarfDieRef`, `LazySymbol`) and
  `DwarfExprEval`. Build `//src/developer/debug/zxdb/symbols` and run `fx test
  zxdb_tests`.
- **`zxdb/expr/`** ([README.md](./zxdb/expr/README.md)): **Layer 3** C++ and
  Rust parser, AST (`ExprNode`), bytecode VM (`VmExec`), and `PrettyType`. Build
  `//src/developer/debug/zxdb/expr` and run `fx test zxdb_tests`.
- **`zxdb/format/`** ([README.md](./zxdb/format/README.md)): **Layer 4** styled
  output buffers (`OutputBuffer`, `AsyncOutputBuffer`) and value or identifier
  formatting. Build `//src/developer/debug/zxdb/format` and run `fx test
  zxdb_tests`.
- **`zxdb/client/`** ([README.md](./zxdb/client/README.md)): **Layer 4**
  UI-agnostic session model (`Session`, `Target`, `Process`, `Thread`, `Stack`,
  `ThreadController`). Build `//src/developer/debug/zxdb/client` and run `fx
  test zxdb_tests`.
- **`zxdb/console/`** ([README.md](./zxdb/console/README.md),
  [commands](./zxdb/console/commands/README.md)): **Layer 5 (UI)** interactive
  CLI frontend, noun and verb parser, and `ConsoleContext`. Build
  `//src/developer/debug/zxdb/console` and run `fx test zxdb_tests`.
- **`zxdb/debug_adapter/`** ([README.md](./zxdb/debug_adapter/README.md),
  [handlers](./zxdb/debug_adapter/handlers/README.md)): **Layer 6 (UI)**
  `cppdap` Debug Adapter Protocol (DAP) server sharing `client::Session`. Build
  `//src/developer/debug/zxdb/debug_adapter` and run `fx test zxdb_tests`.
- **`ffx_zxdb/`** ([README.md](./ffx_zxdb/README.md),
  [src](./ffx_zxdb/src/README.md), [ffx/AGENTS.md](../ffx/AGENTS.md)): Host Rust
  integration for `ffx debug`. Read
  [//src/developer/ffx/AGENTS.md](../ffx/AGENTS.md) for `FDomain` and `fho`
  rules, and build `//src/developer/debug/ffx_zxdb`.
- **`e2e_tests/`** ([README.md](./e2e_tests/README.md)): Host-driven `.script`
  E2E tests and `dap_integration_tests`. Run `fx test --e2e zxdb_e2e_tests`.

Enforce this upward dependency rule across `zxdb/`:

- **Strict Upward Layering in `zxdb/`**: Dependencies inside `zxdb/` must point
  upward (`common` <- `symbols` <- `expr` <- `{format, client}` <- `console` <-
  `debug_adapter`).

## Cross-Cutting Idioms

Apply these C++ and subsystem idioms across all files in
`//src/developer/debug`:

- **Async Object Lifetimes**: Never capture raw pointers (`Target*`, `Process*`,
  `Thread*`, `Frame*`, `Breakpoint*`, `FormatNode*`) across asynchronous
  callbacks. Capture `fxl::WeakPtr<T>` (or `fxl::RefPtr<T>` for ref-counted
  `Symbol`, `EvalContext`, `CommandContext`, `AsyncOutputBuffer`) and check
  validity before dereferencing.
- **Lazy Stack & Multi-Thread State**: Never assume all stack frames exist when
  a thread stops (`debug_agent` sends only the top 1 to 2 frames). Call
  `Stack::SyncFrames` asynchronously for full backtraces, and check
  `process->AllThreadsStopped()` before inspecting process-wide state.
- **Error Returns vs. Invariant Assertions**: Never use `bool` with
  out-parameter pointers for fallible parsing or validation. Return
  `zxdb::ErrOr<T>` (in `zxdb`) or `debug::Status` / `debug::Result<T, E>` (in
  `shared`, `ipc`, `debug_agent`). Reserve `FX_CHECK` and `FX_DCHECK` for
  internal programmer invariants.
- **Sentinel Values (`0`)**: Never use `0` as an invalid sentinel for DWARF unit
  offsets or type signatures. `0` is valid in DWARF, so use
  `std::optional<uint64_t>`. For Zircon KOIDs (`zx_koid_t`), `0`
  (`ZX_KOID_INVALID`) is invalid, so reject `0` when parsing external or DAP
  IDs.
- **Callbacks & Symbol RTTI**: Never use `std::function` or C++ `dynamic_cast`
  (the build disables compiler RTTI). Use `fit::function`, `fit::callback`, or
  `fit::deferred_callback`, and call `symbol->As<T>()` to downcast `Symbol`
  objects.
- **`UncachedLazySymbol::Get()` Lifetime & Empty Symbols**:
  `UncachedLazySymbol::Get()` constructs a new `fxl::RefPtr<Symbol>` on each
  call and returns an empty `Symbol` (`DwarfTag::kNone`) instead of `nullptr`
  when invalid. Before you call `symbol->As<T>()`, store the
  `fxl::RefPtr<Symbol>` in a local variable so the pointer does not dangle. When
  you walk `Symbol::parent()` chains, verify `is_valid()` on the
  `UncachedLazySymbol` or verify `tag() != DwarfTag::kNone` so the loop
  terminates.
- **DWARF-Driven Target Inspection**: When you inspect target runtime data
  structures (for example, in `AsyncTaskProvider` subclasses or `PrettyType`
  formatters), resolve types and members through DWARF symbols and expression
  evaluation. Do not hardcode struct byte offsets. Do not compute raw pointer
  arithmetic against target memory.
- **Shared Utilities & Existing Patterns**: Before you write a file-local helper
  in an anonymous `namespace`, search sibling files and lower `zxdb` or `fxl`
  layers (`common`, `symbols`, `expr`). Follow the patterns in the existing
  code. Reuse lower-layer utilities instead of duplicating them. If a new helper
  is general-purpose, place it in the appropriate lower layer. Add unit tests.
- **Internal Linkage & TODOs**: Place file-local `.cc` helpers in an anonymous
  `namespace`. Format bug links as `https://fxbug.dev/<id>`, and state the
  condition required to delete each `TODO`.
- **Agent Skill Encapsulation**: Never expose internal `ffx debug` or `zxdb`
  Unix socket paths in `skills/` documentation. Direct agents through supported
  CLI commands instead.

## Code Comment Style

Optimize code comments for the reader. Follow these guidelines:

- **Tone and Casing**: Keep comments neutral and factual. Use natural
  contractions (`can't`, `isn't`, `doesn't`). Write non-acronym subsystem or
  protocol names in lowercase (`zircon`, `starnix`, `fidl`, `banjo`).
- **Backticks and Formatting**: Match the formatting style of the surrounding
  file. Do not overuse backticks or parentheses in code comments. Refer to
  qualified function, method, and type names in plain text.
- **Structure and Altitude**: Make the acting component or caller the subject of
  each sentence. State the motivation and intent of a logic block before the
  mechanics. Omit incidental bitmask math, hardcoded struct offsets, exact file
  paths, and change history.
- **Single-Idea Sentences**: Put one main idea in each sentence. Avoid
  multi-clause chains and contrast warmups (such as "It is not X, it is Y").
- **Helper Functions Over Numbered Steps**: Never use numbered lists (`// 1.`,
  `// Step 1:`) in source code comments. If an algorithmic flow has multiple
  phases, split it into named helper functions. Let symbol names show the
  execution order.
- **Architectural Overviews**: Use an explanatory block comment at the top of a
  file or complex sequence. Avoid scattered line comments.

## Downstream Couplings, Documentation, & Reviewer Gates

Before you complete any work, follow these build, documentation, and testing
gates:

- **Dual Bazel/GN Build Files (`# BAZEL2GN SENTINEL`)**: Never hand-edit
  `BUILD.gn` files below a `# BAZEL2GN SENTINEL` marker. Edit the matching
  `BUILD.bazel` file and run `fx bazel2gn`.
- **Documentation Freshness & Prose Style**:
  - Update matching documentation (`//docs/development/debugger/`, local
    `README.md` contracts, and CLI help text in `verbs.h` / `verb_*.cc`)
    whenever you edit user-facing surfaces or internal architecture. Create new
    documentation if none exists.
  - Keep standalone documentation distinct from code comments. Standalone docs
    explain the problem, architecture, and high-level approach. Inline code
    comments explain non-obvious implementation mechanics.
  - Write all non-coding prose in Simplified Technical English (`asd-ste100`)
    and follow Google Technical Writing guidelines (`technical-writing`).
- **Co-locate Unit Tests in Patch Stacks**: Include unit tests in the same CL
  that introduces a new class, data structure, or handler. Never defer unit
  tests to a later commit in a patch stack.
- **Mandatory Boundary, Control-Flow, & Multi-Thread Coverage**:
  - Test zero and invalid boundaries explicitly (CU offset `0`, invalid DWARF
    signatures, `koid == 0`). Test multi-threaded states with 2 or more threads
    (such as `zxdb.AsyncBacktrace` and pause synchronization).
  - In `ThreadController` subclasses and `DebuggedThread` exception paths, test
    early-exit paths (bottom frames, PLT trampolines, unrelated breakpoints).
    Use `fit::defer` or `fit::deferred_callback` to restore state on early
    returns.
  - Never land commented-out test assertions.
- **Editing & Verification Workflow Skill**: Read
  [`editing-zxdb`](./.agents/skills/editing_zxdb/SKILL.md) to build targets, run
  unit and E2E tests, reload `/core/debugger`, and test with `fx debug cli`.
