// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Benchmarks for the `fuchsia-rcu` crate.
//!
//! Measures performance of RCU read scopes (both non-nested and nested) and multi-core atomic bus
//! contention under concurrent reader load.

use fuchsia_criterion::FuchsiaCriterion;
use fuchsia_criterion::criterion::{self as criterion, BenchmarkId, Criterion};
use fuchsia_rcu::RcuReadScope;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

fn main() {
    let mut c = FuchsiaCriterion::default();
    let internal_c: &mut Criterion = &mut c;
    *internal_c = std::mem::take(internal_c)
        .warm_up_time(std::time::Duration::from_millis(100))
        .measurement_time(std::time::Duration::from_millis(1000))
        .sample_size(100);
    const BENCHMARK_SUITE_NAME: &str = if cfg!(rcu_backend = "rseq") {
        "fuchsia.rcu.rseq"
    } else if cfg!(rcu_backend = "atomic") {
        "fuchsia.rcu.atomic"
    } else {
        "fuchsia.rcu"
    };

    let mut group = c.benchmark_group(BENCHMARK_SUITE_NAME);

    // --- Single-threaded Read Scope Benchmarks ---
    let _ = group.bench_function("read_scope/non_nested", bench_read_scope_non_nested);
    let _ = group.bench_function("read_scope/nested", bench_read_scope_nested);

    // --- Multi-Core Contention Benchmarks ---
    // Measures the latency of a single thread's RcuReadScope while N background threads
    // continuously execute non-nested RcuReadScopes in tight loops.
    // In non-RSEQ implementations, concurrent readers on multiple cores contend on the same
    // global atomic read counter via cacheline-invalidating fetch_add/fetch_sub operations.
    // In RSEQ implementations, readers update CPU-local counters with non-atomic operations,
    // avoiding atomic bus synchronization and cacheline bouncing across cores.
    for num_threads in [1, 3, 7] {
        let _ = group.bench_with_input(
            BenchmarkId::new("read_scope/with_contention", num_threads),
            &num_threads,
            |b, &num_threads| bench_read_scope_with_contention(b, num_threads),
        );
    }

    group.finish();
}

// Benchmark creating and immediately dropping a non-nested RcuReadScope.
// This exercises the full read lock acquisition and release path (updating read counters).
fn bench_read_scope_non_nested(bencher: &mut criterion::Bencher<'_>) {
    bencher.iter(|| {
        let scope = RcuReadScope::new();
        let _ = std::hint::black_box(&scope);
        drop(scope);
    });
}

// Benchmark creating and dropping a nested RcuReadScope while an outer scope is held.
// This exercises the nested fast-path (only modifying the thread-local nesting counter).
fn bench_read_scope_nested(bencher: &mut criterion::Bencher<'_>) {
    let _outer = RcuReadScope::new();
    bencher.iter(|| {
        let inner = RcuReadScope::new();
        let _ = std::hint::black_box(&inner);
        drop(inner);
    });
}

/// Measures the latency of a single thread executing `RcuReadScope::new()` while
/// `num_background_threads` continuously execute non-nested read scopes in tight loops.
/// Tests how well a thread performs when the global atomic read counter / memory bus is contested.
fn bench_read_scope_with_contention(
    bencher: &mut criterion::Bencher<'_>,
    num_background_threads: usize,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let ready_barrier = Arc::new(Barrier::new(num_background_threads + 1));

    let mut handles = Vec::with_capacity(num_background_threads);
    for _ in 0..num_background_threads {
        let stop = Arc::clone(&stop);
        let ready = Arc::clone(&ready_barrier);
        handles.push(std::thread::spawn(move || {
            let _ = ready.wait();
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..100 {
                    let scope = RcuReadScope::new();
                    let _ = std::hint::black_box(&scope);
                    drop(scope);
                }
            }
        }));
    }

    let _ = ready_barrier.wait();

    bencher.iter(|| {
        let scope = RcuReadScope::new();
        let _ = std::hint::black_box(&scope);
        drop(scope);
    });

    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.join();
    }
}
