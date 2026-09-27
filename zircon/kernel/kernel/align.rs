// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[inline(always)]
pub const fn roundup(a: usize, b: usize) -> usize {
    (a + (b - 1)) & !(b - 1)
}

#[inline(always)]
pub const fn rounddown(a: usize, b: usize) -> usize {
    a & !(b - 1)
}

#[inline(always)]
pub const fn is_rounded(a: usize, b: usize) -> bool {
    (a & (b - 1)) == 0
}
