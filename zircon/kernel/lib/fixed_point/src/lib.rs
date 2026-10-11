// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Fixed point arithmetic helpers mirroring `<lib/fixed_point.h>`.
//!
//! The central type is [`Fp3264`], a 32.64 unsigned fixed point number (32
//! integer bits and 64 fractional bits) used by platform timer code to convert
//! between clock domains (e.g. TSC ticks to nanoseconds) without floating
//! point.

#![no_std]

mod fixed_point;
mod fixed_point_debug;

pub use fixed_point::{
    Fp3264, fp_32_64_div_32_32, u32_mul_u64_fp32_64, u64_mul_u32_fp32_64, u64_mul_u64_fp32_64,
};
