// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

//! Testing utilities for creating fakes for MMIO driver library.
//!
//! See the [`inject`] module for details on the dependency-injection patterns
//! provided by this crate.

mod cached_vmo;
pub mod inject;
mod operand;

pub use cached_vmo::CachedVmoMemory;
pub use operand::MmioOperand;
