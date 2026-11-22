// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod attach;
mod boot_and_accessories;
mod cc;
mod concurrency;
mod disconnect;
mod fixture;
mod inspect;
mod spec;
mod vbus;
mod vbus_discharge;

pub(in crate::tests) use crate::port::{
    DEFAULT_DISCHARGE_TIMEOUT, DEFAULT_T_CC_DEBOUNCE, DEFAULT_T_PD_DEBOUNCE,
    DEFAULT_T_RP_VALUE_CHANGE, DEFAULT_T_SRC_DISCONNECT, DEFAULT_VSAFE0V_DEBOUNCE,
};
pub(in crate::tests) use crate::power::PowerPathState;
pub(in crate::tests) use crate::*;
pub(in crate::tests) use anyhow::Result;
pub(in crate::tests) use assert_matches::assert_matches;
pub(in crate::tests) use diagnostics_assertions::{AnyProperty, assert_data_tree};
pub(in crate::tests) use fidl_next_fuchsia_hardware_usb_connector as fconnector;
pub(in crate::tests) use fixture::*;
pub(in crate::tests) use fuchsia_async as fasync;
pub(in crate::tests) use fuchsia_sync::Mutex;
pub(in crate::tests) use futures::channel::oneshot;
pub(in crate::tests) use futures::lock::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};
pub(in crate::tests) use futures::poll;
pub(in crate::tests) use std::future::Future;
pub(in crate::tests) use std::pin::{Pin, pin};
pub(in crate::tests) use std::sync::Arc;
pub(in crate::tests) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
pub(in crate::tests) use std::task::Poll;
