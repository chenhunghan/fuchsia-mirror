// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Host Controller Interface (HCI) layer of the Sapphire Bluetooth stack.
//!
//! [`hci::HciResources`] holds the state of the connection between the host and a Bluetooth
//! controller over an HCI transport ([`transport`]). It splits into an [`hci::HciRunner`], which
//! drives the controller, and an [`hci::HciHandle`], which holds an [`hci::CommandClient`] to
//! send commands and an [`hci::EventClient`] to subscribe to the events the controller sends back
//! ([`events`]).
//!
//! The crate is `#![no_std]` and does not allocate on the heap.

#![no_std]
#![allow(unused_crate_dependencies)]

#[cfg(test)]
extern crate std;

pub mod command;
pub mod events;
pub mod hci;
pub mod transport;

#[cfg(test)]
mod testing;
