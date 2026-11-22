// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Microbenchmarks for the `fuchsia-async` executor.
//!
//! Measures single-threaded (`LocalExecutor`) and multi-threaded (`SendExecutor`) scheduling
//! throughput, channel wakeup locality, cross-worker task spawning, and per-task CPU cache
//! working-set locality across polls.

use crossbeam_utils::CachePadded;
use fuchsia_async::{LocalExecutor, Scope, SendExecutorBuilder, yield_now};
use fuchsia_criterion::FuchsiaCriterion;
use fuchsia_criterion::criterion::BenchmarkId;
use fuchsia_sync::Mutex;
use futures::StreamExt;
use futures::channel::mpsc;
use std::future::Future;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

const YIELD_TASKS: usize = 64;
const YIELDS_PER_TASK: usize = 64;
const PING_PONG_MESSAGES: usize = 128;
const SPAWN_JOIN_TASKS: usize = 64;

/// Number of `u64` elements in each task's working set (4096 * 8 bytes = 32 KiB, 512 cache lines).
const WORKING_SET_U64S: usize = 4096;
/// Step in `u64` elements between cache lines (8 * 8 bytes = 64 bytes).
const CACHE_LINE_STEP: usize = 8;

fn main() {
    let mut c = FuchsiaCriterion::default();
    let mut group = c.benchmark_group("fuchsia.async");
    let _ = group
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(800))
        .sample_size(50);

    let _ = group.bench_function("local_executor/yield_batch", {
        let mut exec = None;
        move |b| {
            let exec = exec.get_or_insert_with(LocalExecutor::default);
            b.iter(|| exec.run_singlethreaded(run_yield_batch()));
        }
    });
    let _ = group.bench_function("local_executor/channel_ping_pong", {
        let mut exec = None;
        move |b| {
            let exec = exec.get_or_insert_with(LocalExecutor::default);
            b.iter(|| exec.run_singlethreaded(run_channel_ping_pong(4)));
        }
    });

    for threads in [2u8, 4u8] {
        let _ = group.bench_with_input(
            BenchmarkId::new("send_executor/yield_batch", threads),
            &threads,
            {
                let mut runner = None;
                move |b, &threads| {
                    let runner =
                        runner.get_or_insert_with(|| SendRunner::new(threads, run_yield_batch));
                    b.iter_custom(|iters| runner.run_iters(iters));
                }
            },
        );
        let _ = group.bench_with_input(
            BenchmarkId::new("send_executor/channel_ping_pong", threads),
            &threads,
            {
                let mut runner = None;
                move |b, &threads| {
                    let runner = runner.get_or_insert_with(|| {
                        let pairs = threads as usize;
                        SendRunner::new(threads, move || run_channel_ping_pong(pairs))
                    });
                    b.iter_custom(|iters| runner.run_iters(iters));
                }
            },
        );
        let _ = group.bench_with_input(
            BenchmarkId::new("send_executor/working_set_locality", threads),
            &threads,
            {
                let mut runner = None;
                move |b, &threads| {
                    let runner =
                        runner.get_or_insert_with(|| new_working_set_locality_runner(threads));
                    b.iter_custom(|iters| runner.run_iters(iters));
                }
            },
        );
        let _ = group.bench_with_input(
            BenchmarkId::new("send_executor/spawn_join", threads),
            &threads,
            {
                let mut runner = None;
                move |b, &threads| {
                    let runner = runner.get_or_insert_with(|| {
                        SendRunner::new(threads, || async {
                            let scope = Scope::new();
                            for i in 0..SPAWN_JOIN_TASKS {
                                let _ = scope.spawn(async move {
                                    let _ = black_box(i);
                                });
                            }
                            scope.await;
                        })
                    });
                    b.iter_custom(|iters| runner.run_iters(iters));
                }
            },
        );
    }

    group.finish();
}

/// Keeps a `SendExecutor` worker pool alive across Criterion warm-up and measurement samples so
/// benchmarks measure steady-state task scheduling without OS thread teardown or cross-thread
/// harness wakeups on each iteration.
struct SendRunner {
    iter_tx: mpsc::UnboundedSender<u64>,
    done_rx: std::sync::mpsc::Receiver<Duration>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SendRunner {
    fn new<F, Fut>(num_threads: u8, mut make_iter: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (iter_tx, mut iter_rx) = mpsc::unbounded::<u64>();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<Duration>(1);
        let thread = std::thread::spawn(move || {
            let mut exec = SendExecutorBuilder::new().num_threads(num_threads).build();
            exec.run(async move {
                ready_tx.send(()).expect("signal executor ready");
                while let Some(iters) = iter_rx.next().await {
                    let start = Instant::now();
                    for _ in 0..iters {
                        make_iter().await;
                    }
                    done_tx.send(start.elapsed()).expect("signal sample complete");
                }
            });
        });
        ready_rx.recv().expect("wait for executor ready");
        Self { iter_tx, done_rx, thread: Some(thread) }
    }

    #[inline]
    fn run_iters(&self, iters: u64) -> Duration {
        self.iter_tx.unbounded_send(iters).expect("send sample iteration count");
        self.done_rx.recv().expect("wait for sample completion")
    }
}

impl Drop for SendRunner {
    fn drop(&mut self) {
        self.iter_tx.disconnect();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run_yield_batch() {
    let scope = Scope::new();
    for _ in 0..YIELD_TASKS {
        let _ = scope.spawn(async {
            for i in 0..YIELDS_PER_TASK {
                let _ = black_box(i);
                yield_now().await;
            }
        });
    }
    scope.await;
}

async fn run_channel_ping_pong(pairs: usize) {
    let scope = Scope::new();
    for _ in 0..pairs {
        let (mut tx_a, mut rx_a) = mpsc::channel::<usize>(1);
        let (mut tx_b, mut rx_b) = mpsc::channel::<usize>(1);
        let _ = scope.spawn(async move {
            for i in 0..PING_PONG_MESSAGES {
                tx_b.try_send(i).expect("send ping");
                let reply = rx_a.next().await.expect("receive pong");
                let _ = black_box(reply);
            }
        });
        let _ = scope.spawn(async move {
            while let Some(msg) = rx_b.next().await {
                tx_a.try_send(msg.wrapping_add(1)).expect("send pong");
            }
        });
    }
    scope.await;
}

fn new_working_set_locality_runner(num_threads: u8) -> SendRunner {
    let num_tasks = (num_threads as usize) * 2;
    let buffers: Arc<Vec<Arc<CachePadded<Mutex<Box<[u64; WORKING_SET_U64S]>>>>>> = Arc::new(
        (0..num_tasks)
            .map(|i| Arc::new(CachePadded::new(Mutex::new(Box::new([i as u64; WORKING_SET_U64S])))))
            .collect(),
    );

    SendRunner::new(num_threads, move || {
        let buffers = Arc::clone(&buffers);
        async move {
            let scope = Scope::new();
            for buf in buffers.iter().cloned() {
                let _ = scope.spawn(async move {
                    for _ in 0..YIELDS_PER_TASK {
                        {
                            let mut data = buf.lock();
                            let mut idx = 0;
                            while idx < WORKING_SET_U64S {
                                data[idx] = data[idx].wrapping_add(1);
                                idx += CACHE_LINE_STEP;
                            }
                            let _ = black_box(data[0]);
                        }
                        yield_now().await;
                    }
                });
            }
            scope.await;
        }
    })
}
