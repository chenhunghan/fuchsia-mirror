// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Source of test_readelf_output.json, which is the llvm-readelf output for
// a linked file built from this program.
//
// Build a linked file, so that addresses are final:
//   clang --target=x86_64-unknown-linux-gnu -shared -nostdlib -fPIC -O1 \
//     -mno-red-zone -fexperimental-call-graph-section -fstack-size-section \
//     -fvisibility=default -Wl,-Bsymbolic -o test_readelf_output.so \
//     test_readelf_output.c
//
// Dump, with an llvm-readelf that prints the address of each stack size
// entry:
//   llvm-readelf --elf-output-style=JSON --pretty-print --call-graph-info \
//     --stack-sizes --dyn-symbols test_readelf_output.so \
//     > test_readelf_output.json

extern void external_function(char *buf);

static char volatile sink[64];

__attribute__((noinline)) int leaf(int x) {
  char buf[16];
  for (int i = 0; i < 16; ++i)
    buf[i] = (char)(x + i);
  for (int i = 0; i < 16; ++i)
    sink[i] = buf[i];
  return x + 1;
}

__attribute__((noinline)) int helper(int x) {
  char buf[32];
  for (int i = 0; i < 32; ++i)
    buf[i] = (char)(x - i);
  for (int i = 0; i < 32; ++i)
    sink[i] = buf[i];
  return leaf(x) + 2;
}

// Two names for the same function.
int helper_alias(int x) __attribute__((alias("helper")));

// Mutual recursion, which the optimizer can't turn into a loop.
int rec_b(int n);

__attribute__((noinline)) int rec_a(int n) {
  char buf[8];
  buf[n & 7] = (char)n;
  sink[n & 7] = buf[n & 7];
  return n <= 0 ? 0 : rec_b(n - 1) + leaf(buf[1]);
}

__attribute__((noinline)) int rec_b(int n) {
  char buf[24];
  buf[n & 15] = (char)n;
  sink[n & 15] = buf[n & 15];
  return n <= 0 ? 0 : rec_a(n - 1) + leaf(buf[2]);
}

// Dynamically sized stack object: Clang emits no stack size entry. The
// callee is in another module, so its address is 0 in the linked file.
__attribute__((noinline)) int vla(int n) {
  char buf[n];
  for (int i = 0; i < n; ++i)
    buf[i] = (char)i;
  external_function(buf);
  return buf[0];
}

// Indirect call targets, with a type that no other function has.
typedef int (*binary_fn)(int, int);

__attribute__((noinline)) int add(int x, int y) {
  char buf[40];
  for (int i = 0; i < 40; ++i)
    buf[i] = (char)(x + y + i);
  for (int i = 0; i < 40; ++i)
    sink[i] = buf[i];
  return x + y;
}

__attribute__((noinline)) int mul(int x, int y) {
  char buf[64];
  for (int i = 0; i < 64; ++i)
    buf[i] = (char)(x * y + i);
  for (int i = 0; i < 64; ++i)
    sink[i] = buf[i];
  return x * y;
}

__attribute__((noinline)) int call_through(binary_fn fn, int x) {
  char buf[48];
  for (int i = 0; i < 48; ++i)
    buf[i] = (char)x;
  for (int i = 0; i < 48; ++i)
    sink[i] = buf[i];
  return fn(x, buf[3]);
}

int entry(int x) {
  char buf[128];
  for (int i = 0; i < 128; ++i)
    buf[i] = (char)x;
  for (int i = 0; i < 64; ++i)
    sink[i] = buf[i * 2];
  int r = helper(x) + rec_a(x) + vla(x & 15);
  r += call_through(x & 1 ? add : mul, x);
  return r;
}
