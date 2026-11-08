// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod api;
pub mod converter;
pub mod error;
pub mod executor;
pub mod maps;
pub mod program;
pub mod scalar_value;
pub mod verifier;
pub mod visitor;

pub use api::*;
pub use converter::*;
pub use error::*;
pub use executor::*;
pub use maps::*;
pub use program::*;
pub use verifier::{
    AluLogOp, AtomicLogOp, CoreCallingContext, CoreVerifierLogger, FieldDescriptor, FieldType,
    FunctionSignature, FunctionSignatureBuilder, JmpLogOp, MAX_PACKET_OFFSET, MapTypeFilter,
    MemoryId, MemoryParameterSize, NullCoreVerifierLogger, PointerOrigin, StructAccess,
    StructDescriptor, Type, VerifiedEbpfProgram, VerifierLogEvent, verify_program_core,
};
