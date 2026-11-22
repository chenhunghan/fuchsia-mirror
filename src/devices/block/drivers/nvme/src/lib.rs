// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! NVM Express (NVMe) PCIe block storage driver for Fuchsia (DFv2).

#![deny(clippy::await_holding_lock)]

pub mod commands;
mod inspect;
pub mod io_processor;
pub mod namespace;
pub mod queue;
pub mod queue_pair;
pub mod registers;
