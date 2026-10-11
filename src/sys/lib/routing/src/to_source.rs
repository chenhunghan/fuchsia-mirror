// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use cm_rust::{
    DebugRegistration, ExposeDecl, ExposeDeclCommon, GenericRef, OfferDecl, OfferDeclCommon,
    ResolverRegistration, RunnerRegistration, ToGenericRef, UseDecl, UseDeclCommon,
};

pub trait ToSource {
    fn to_source(&self) -> GenericRef;
}

impl ToSource for UseDecl {
    fn to_source(&self) -> GenericRef {
        self.source().clone().to_generic()
    }
}

impl ToSource for OfferDecl {
    fn to_source(&self) -> GenericRef {
        self.source().clone().to_generic()
    }
}

impl ToSource for ExposeDecl {
    fn to_source(&self) -> GenericRef {
        self.source().clone().to_generic()
    }
}

impl ToSource for DebugRegistration {
    fn to_source(&self) -> GenericRef {
        let DebugRegistration::Protocol(debug) = self;
        debug.source.clone().to_generic()
    }
}

impl ToSource for RunnerRegistration {
    fn to_source(&self) -> GenericRef {
        self.source.clone().to_generic()
    }
}

impl ToSource for ResolverRegistration {
    fn to_source(&self) -> GenericRef {
        self.source.clone().to_generic()
    }
}
