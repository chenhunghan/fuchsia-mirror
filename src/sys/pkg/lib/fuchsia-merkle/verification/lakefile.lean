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
@[default_target] lean_lib «FuchsiaMerkleExtracted» {
  srcDir := "proof"
  roots := #[`Extracted]
}

-- 2. Security-critical cryptographic hardness assumptions and external trait contracts.
--    Cannot import Spec, Proof, or Contract.
@[default_target] lean_lib «FuchsiaMerkleAssumptions» {
  srcDir := "spec"
  roots := #[`Assumptions]
}

-- 3. Security-critical mathematical specification of the Fuchsia Merkle Tree protocol.
--    May import Assumptions and Extracted; cannot import Proof or Contract.
@[default_target] lean_lib «FuchsiaMerkleSpec» {
  srcDir := "spec"
  roots := #[`Spec]
}

-- 4. Engineer-editable proof module containing invariants, helper lemmas, and proof bodies.
@[default_target] lean_lib «FuchsiaMerkleProof» {
  srcDir := "proof"
  globs := #[.andSubmodules `Proof]
}

-- 5. Security-critical top-level theorem statements and `#print axioms` audit.
@[default_target] lean_lib «FuchsiaMerkleContract» {
  srcDir := "spec"
  roots := #[`Contract]
}
