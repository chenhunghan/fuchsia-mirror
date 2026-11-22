// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_BITS_H_
#define ZIRCON_KERNEL_INCLUDE_BITS_H_

/* Trick to get a 1 of the right size */
#define _ONE(x) (1 + ((x) - (x)))

#define BIT(x, bit) ((x) & (_ONE(x) << (bit)))
#define BIT_SHIFT(x, bit) (((x) >> (bit)) & 1)
#define BITS(x, high, low) ((x) & (((_ONE(x) << ((high) + 1)) - 1) & ~((_ONE(x) << (low)) - 1)))
#define BITS_SHIFT(x, high, low) (((x) >> (low)) & ((_ONE(x) << ((high) - (low) + 1)) - 1))

#define BIT_MASK(x) (((x) >= sizeof(unsigned long) * 8) ? (0UL - 1) : ((1UL << (x)) - 1))
#define BIT_MASK32(x) (((x) >= sizeof(unsigned) * 8) ? (0U - 1) : ((1U << (x)) - 1))

#endif  // ZIRCON_KERNEL_INCLUDE_BITS_H_
