-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Lake
open Lake DSL

-- Important: mathlib imports std4 and quote4; do not add a separate `require std4`.
require mathlib from git
  "https://github.com/leanprover-community/mathlib4.git" @ "v4.35.0"

package «aeneas» where
  preferReleaseBuild := true
  buildArchive := s!"lean-build-aeneas-{System.Platform.target}.tar.gz"
  leanOptions := #[⟨`weak.linter.dupNamespace, false⟩]

@[default_target] lean_lib «Aeneas» {}

private def notCI : Bool := run_io
  return (← IO.getEnv "CI").isNone

@[default_target] lean_lib «AeneasMeta» {
  precompileModules := notCI
}

-- 1. Auto-extracted Rust model produced by Charon + Aeneas in `proof/Extracted.lean`.
@[default_target] lean_lib «StarnixEbpfExtracted» {
  srcDir := "proof"
  roots := #[`Extracted]
}

-- 2. Security-critical assumptions on external traits, host layout, and calling context.
--    Cannot import Spec, Proof, or Contract.
@[default_target] lean_lib «StarnixEbpfAssumptions» {
  srcDir := "spec"
  roots := #[`Assumptions]
}

-- 3. Security-critical specification of the eBPF virtual machine and safety properties.
--    May import Assumptions and Extracted; cannot import Proof or Contract.
@[default_target] lean_lib «StarnixEbpfSpec» {
  srcDir := "spec"
  roots := #[`Spec]
}

-- 4. Engineer-editable proof module containing invariants, helper lemmas, and proof bodies.
@[default_target] lean_lib «StarnixEbpfProof» {
  srcDir := "proof"
  globs := #[.andSubmodules `Proof]
}

-- 5. Security-critical top-level theorem statements and `#print axioms` audit.
@[default_target] lean_lib «StarnixEbpfContract» {
  srcDir := "spec"
  roots := #[`Contract]
}
