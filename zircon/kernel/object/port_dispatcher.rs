// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::impl_dispatcher_facade;
use zx_types::ZX_OBJ_TYPE_PORT;

impl_dispatcher_facade!(
    /// Facade for C++ `PortDispatcher`.
    pub struct PortDispatcher,
    ZX_OBJ_TYPE_PORT
);
