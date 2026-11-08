// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hardware-agnostic USB Type-C Port Manager (TCPM) CC decoding and port state machine.
//!
//! | TCPC driver's role | Item |
//! | :-- | :-- |
//! | Implements | [`PortController`]: port controller hardware operations |
//! | Implements | [`Typec`]: partner callbacks |
//! | Embeds | [`TypecPort`]: port configuration and synchronized state |
//! | Calls | [`TypecExt`]: state queries and Inspect diagnostics |
//! | Reports | [`CcStatus`] and [`RoleControl`]: CC line state and presented terminations |

mod cc;
mod port;
mod state;
mod typec;

pub use cc::{Accessory, CcPinState, CcStatus, CcTermination, PowerRole, RoleControl, RpValue};
pub use port::{DebounceDirection, TypecPort, TypecState};
pub use typec::{PortController, Typec, TypecExt};

#[cfg(test)]
mod tests;
