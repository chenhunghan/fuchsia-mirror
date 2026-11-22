// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stddef.h>

#include <explicit-memory/bytes.h>
#include <openssl/mem.h>

#include "third_party/boringssl/src/crypto/fipsmodule/sha/sha256.cc.inc"

// This file contains the .inc includes and stub definitions needed for a
// minimal, kernel-appropriate port of boringssl code.

// See //third_party/boringssl/src/crypto/mem.cc.
// Use mandatory_memset to ensure zeroing is not optimized away.
void OPENSSL_cleanse(void* ptr, size_t len) {
  if (len != 0) {
    mandatory_memset(ptr, 0, len);
  }
}
