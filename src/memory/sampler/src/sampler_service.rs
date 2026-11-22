// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This modules contains most of the top-level logic of the sampler
//! service. It defines functions to process a stream of profiling
//! requests and produce a complete profile, as well as utilities to
//! persist it.
use crate::crash_reporter::{ProfileReport, ProfileSender};
use crate::profile_builder::ProfileBuilder;

use anyhow::Error;
use fidl_fuchsia_memory_sampler::{
    RecordAllocationEvent, RecordDeallocationEvent, SamplerRequest, SamplerRequestStream,
    SamplerSetProcessInfoRequest,
};
use fuchsia_async::Task;
use fuchsia_component::server::ServiceFs;
use futures::prelude::*;
use futures::stream::SelectAll;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Upper bound on the reclaimable stack traces buffered for a single process
/// before flushing a partial profile. Measured on production profiles (mean
/// depth ~27 frames), each unique stack trace costs ~300 B; keeping this at
/// 3000 keeps the residual memory *for a single profiled process* roughly
/// under ~1 MiB.
const RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD: usize = 3000;

/// Shared upper bound on the total reclaimable stack traces held across all
/// active connections before partial profiles are flushed (~10 MiB).
const TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET: usize =
    10 * RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD;

/// Upper bound on the elapsed time between producing two partial
/// profiles.
const MAX_DURATION_BETWEEN_PARTIAL_PROFILES: Duration = Duration::from_secs(12 * 60 * 60);

/// Aggregates buffered reclaimable stack-trace counts across all open sessions.
#[derive(Clone, Copy, Debug, Default)]
struct TotalBudget {
    /// Number of sessions currently holding `reclaimable_stack_traces > 0`.
    contributing_sessions: usize,
    /// Total number of unique dead-allocation and deallocation stack traces
    /// buffered across all sessions (entries that building a partial profile
    /// reclaims from [`ProfileBuilder`]).
    reclaimable_stack_traces: usize,
}

/// Shared tracker bounding the total reclaimable stack traces buffered across
/// all concurrently profiled processes.
#[derive(Clone, Debug, Default)]
struct StackTraceBudget {
    total: Rc<Cell<TotalBudget>>,
}

impl StackTraceBudget {
    fn enter(&self) -> SessionStackTraceBudget {
        SessionStackTraceBudget { total: self.total.clone(), previous_reclaimable_stack_traces: 0 }
    }
}

/// RAII handle tracking one connection's contribution to [`StackTraceBudget`].
struct SessionStackTraceBudget {
    total: Rc<Cell<TotalBudget>>,
    /// Memorizes this session's previous contribution to [`TotalBudget`] so the
    /// shared totals can be updated incrementally and subtracted on drop.
    previous_reclaimable_stack_traces: usize,
}

impl SessionStackTraceBudget {
    /// Updates this session's reclaimable stack-trace count in the shared
    /// budget and returns whether the session should flush a partial profile.
    fn update(&mut self, current_reclaimable: usize) -> bool {
        self.total.update(|mut total| {
            total.contributing_sessions = total.contributing_sessions
                - usize::from(self.previous_reclaimable_stack_traces > 0)
                + usize::from(current_reclaimable > 0);
            total.reclaimable_stack_traces = total.reclaimable_stack_traces
                - self.previous_reclaimable_stack_traces
                + current_reclaimable;
            total
        });
        self.previous_reclaimable_stack_traces = current_reclaimable;
        let total = self.total.get();

        if current_reclaimable >= RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD {
            return true;
        }
        let fair_share =
            (TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET / total.contributing_sessions.max(1)).max(1);
        total.reclaimable_stack_traces >= TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET
            && current_reclaimable >= fair_share
    }
}

impl Drop for SessionStackTraceBudget {
    fn drop(&mut self) {
        if self.previous_reclaimable_stack_traces > 0 {
            self.total.update(|mut total| {
                total.contributing_sessions -= 1;
                total.reclaimable_stack_traces -= self.previous_reclaimable_stack_traces;
                total
            });
        }
    }
}

/// Marks `builder` lossy if the client has signalled that it dropped datagrams
/// because the shared socket's buffer was saturated. The signal latches on the
/// socket, so polling it at profile emission time is sufficient and keeps the
/// syscall off the per-datagram path.
fn check_lossy_signal(socket: &Option<zx::Socket>, builder: &mut ProfileBuilder) {
    if builder.is_lossy() {
        return;
    }
    let Some(socket) = socket else {
        return;
    };
    if let zx::WaitResult::Ok(signals) =
        socket.wait_one(zx::Signals::USER_0, zx::MonotonicInstant::ZERO)
        && signals.contains(zx::Signals::USER_0)
    {
        builder.set_lossy(true);
    }
}

fn maybe_emit_partial_profile(
    builder: &mut ProfileBuilder,
    tx: &ProfileSender,
    budget: &mut SessionStackTraceBudget,
    time_of_last_profile: Instant,
    shared_socket: &Option<zx::Socket>,
) -> Result<Option<Instant>, Error> {
    let now = Instant::now();
    let should_flush = budget.update(builder.get_approximate_reclaimable_stack_traces_count());
    if (now - time_of_last_profile >= MAX_DURATION_BETWEEN_PARTIAL_PROFILES) || should_flush {
        check_lossy_signal(shared_socket, builder);
        let profile = builder.build_partial_profile()?;
        let _ = budget.update(builder.get_approximate_reclaimable_stack_traces_count());
        let res = tx.try_send(profile);
        if let Err(ref e) = res
            && e.is_full()
        {
            log::warn!(
                "{}: Failed to send partial profile, channel is full; disconnecting client",
                builder.process_name()
            );
            return Ok(None);
        };
        res?;
        return Ok(Some(now));
    }
    Ok(Some(time_of_last_profile))
}

enum ClientEvent {
    RecordAllocation(RecordAllocationEvent),
    RecordDeallocation(RecordDeallocationEvent),
    SetProcessInfo(SamplerSetProcessInfoRequest),
    SetSharedSocket(zx::Socket),
    Ignored,
    PeerClosed,
}

impl From<&[u8]> for ClientEvent {
    fn from(datagram: &[u8]) -> ClientEvent {
        if datagram.is_empty() {
            // `fuchsia_async::Socket::poll_datagram` can yield a 0-byte read on
            // an open socket if `outstanding_read_bytes()` observes 0 bytes
            // immediately before a datagram arrives and `zx_socket_read` is
            // called with a 0-length buffer. The datagram remains in the kernel
            // socket buffer and will be read on the next poll.
            return ClientEvent::Ignored;
        }
        match fidl::unpersist::<fidl_fuchsia_memory_sampler::SamplerDatagram>(&datagram) {
            Ok(fidl_fuchsia_memory_sampler::SamplerDatagram::RecordAllocation(alloc)) => {
                ClientEvent::RecordAllocation(alloc)
            }
            Ok(fidl_fuchsia_memory_sampler::SamplerDatagram::RecordDeallocation(dealloc)) => {
                ClientEvent::RecordDeallocation(dealloc)
            }
            Ok(fidl_fuchsia_memory_sampler::SamplerDatagramUnknown!()) => {
                log::warn!("Received unknown SamplerDatagram variant");
                ClientEvent::Ignored
            }
            Err(e) => {
                log::warn!("Failed to handle datagram: {:#}", e);
                ClientEvent::Ignored
            }
        }
    }
}

impl From<SamplerRequest> for ClientEvent {
    fn from(request: SamplerRequest) -> ClientEvent {
        match request {
            SamplerRequest::SetSharedSocket { socket, .. } => ClientEvent::SetSharedSocket(socket),
            SamplerRequest::RecordAllocation { payload, .. } => {
                ClientEvent::RecordAllocation(payload)
            }
            SamplerRequest::RecordDeallocation { payload, .. } => {
                ClientEvent::RecordDeallocation(payload)
            }
            SamplerRequest::SetProcessInfo { payload, .. } => ClientEvent::SetProcessInfo(payload),
            unknown_method => {
                log::debug!("Unknown, unhandled method: {:?}", unknown_method);
                ClientEvent::Ignored
            }
        }
    }
}

/// Build a profile from a stream of profiling requests and socket datagrams.
///
/// Returns `Ok(None)` if profiling is aborted because the crash reporter
/// channel is full when submitting a partial profile, dropping the FIDL stream
/// and shared socket so the client can disconnect.
async fn process_sampler_requests(
    stream: SamplerRequestStream,
    tx: &ProfileSender,
    mut budget: SessionStackTraceBudget,
) -> Result<Option<ProfileReport>, Error> {
    let mut profile_builder = ProfileBuilder::default();
    let mut time_of_last_profile = Instant::now();
    let mut shared_socket: Option<zx::Socket> = None;

    let mut event_streams: SelectAll<
        futures::stream::BoxStream<'static, Result<ClientEvent, Error>>,
    > = SelectAll::new();

    let fidl_stream = stream
        .map(|res| match res {
            Ok(request) => Ok(ClientEvent::from(request)),
            Err(e) => Err(anyhow::Error::from(e).context("failed fidl request")),
        })
        .boxed();

    event_streams.push(fidl_stream);

    while let Some(event_res) = event_streams.next().await {
        let event = event_res?;
        if matches!(event, ClientEvent::PeerClosed) {
            log::info!("Socket connection closed by peer");
            break;
        }

        match event {
            ClientEvent::SetSharedSocket(socket) => {
                // A client is expected to install its socket exactly once, but
                // do not depend on that: harvest any saturation signal latched
                // on the socket being replaced, since dropping its handle here
                // would lose the signal permanently.
                check_lossy_signal(&shared_socket, &mut profile_builder);
                shared_socket = match socket.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!("Failed to duplicate shared socket handle: {:#}", e);
                        None
                    }
                };
                let socket_stream = fuchsia_async::Socket::from_socket(socket)
                    .into_datagram_stream()
                    .map(|res| match res {
                        Ok(datagram) => Ok(ClientEvent::from(&datagram[..])),
                        Err(e) => Err(anyhow::Error::from(e).context("failed socket read")),
                    })
                    .chain(futures::stream::once(async { Ok(ClientEvent::PeerClosed) }))
                    .boxed();
                event_streams.push(socket_stream);
            }
            ClientEvent::RecordAllocation(RecordAllocationEvent {
                address,
                stack_trace,
                size,
                sampling_interval_bytes,
                ..
            }) => {
                let address = address.ok_or_else(|| {
                    anyhow::anyhow!("Unsupported record allocation request: missing address")
                })?;
                let stack_frames = stack_trace
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Unsupported record allocation request: missing stack_trace"
                        )
                    })?
                    .stack_frames
                    .ok_or_else(|| {
                        anyhow::anyhow!("Unsupported stack trace: missing stack frames")
                    })?;
                let size = size.ok_or_else(|| {
                    anyhow::anyhow!("Unsupported record allocation request: missing size")
                })?;
                profile_builder.allocate(address, stack_frames, size, sampling_interval_bytes);
            }
            ClientEvent::RecordDeallocation(RecordDeallocationEvent {
                address,
                stack_trace,
                ..
            }) => {
                let address = address.ok_or_else(|| {
                    anyhow::anyhow!("Unsupported deallocation request: missing address")
                })?;
                let stack_frames = stack_trace
                    .ok_or_else(|| {
                        anyhow::anyhow!("Unsupported deallocation request: missing stack trace")
                    })?
                    .stack_frames
                    .ok_or_else(|| {
                        anyhow::anyhow!("Unsupported stack_trace: missing stack frames")
                    })?;
                profile_builder.deallocate(address, stack_frames);
            }
            ClientEvent::SetProcessInfo(SamplerSetProcessInfoRequest {
                process_name,
                module_map,
                ..
            }) => {
                profile_builder.set_process_info(process_name, module_map.into_iter().flatten());
            }
            ClientEvent::PeerClosed => unreachable!(),
            ClientEvent::Ignored => continue,
        }

        let Some(new_time) = maybe_emit_partial_profile(
            &mut profile_builder,
            tx,
            &mut budget,
            time_of_last_profile,
            &shared_socket,
        )?
        else {
            return Ok(None);
        };
        time_of_last_profile = new_time;
    }

    check_lossy_signal(&shared_socket, &mut profile_builder);

    Ok(Some(profile_builder.build()?))
}

/// Serves the `Sampler` protocol for a given process. Once a client
/// closes their connection, enqueues a `pprof`-compatible profile to
/// the provided channel, for further processing. May also regularly
/// enqueue partial profiles, to offload the profiler's memory usage.
///
/// Note: this function does not retry pushing profiles through the
/// channel. Failure to handle profile reports in a timely manner will
/// cause this component to shut down.
async fn run_sampler_service(
    stream: SamplerRequestStream,
    tx: ProfileSender,
    budget: SessionStackTraceBudget,
) -> Result<(), Error> {
    let Some(profile) = process_sampler_requests(stream, &tx, budget).await? else {
        return Ok(());
    };
    let process_name = profile.process_name().to_string();
    log::debug!("Profiling for {} done, queuing final report", process_name);
    {
        let res = tx.try_send(profile);
        if let Err(ref e) = res
            && e.is_full()
        {
            log::warn!("{}: Failed to send final profile, channel is full", process_name);
        };
    }
    Ok(())
}

enum IncomingServiceRequest {
    Sampler(SamplerRequestStream),
}

async fn serve_sampler_requests(
    requests: impl Stream<Item = IncomingServiceRequest>,
    tx: ProfileSender,
) -> Result<(), Error> {
    let budget = StackTraceBudget::default();
    requests
        .map(Ok)
        .try_for_each_concurrent(None, move |IncomingServiceRequest::Sampler(stream)| {
            run_sampler_service(stream, tx.clone(), budget.enter())
        })
        .inspect_err(|e| log::error!("fuchsia.memory.sampler/Sampler protocol: {}", e))
        .await
}

/// Returns a task that serves the `fuchsia.memory.sampler/Sampler`
/// protocol.
///
/// Note: any error will cause the sampler service to shutdown.
pub fn setup_sampler_service(tx: ProfileSender) -> Result<Task<Result<(), Error>>, Error> {
    let mut service_fs = ServiceFs::new();
    service_fs.dir("svc").add_fidl_service(IncomingServiceRequest::Sampler);
    service_fs.take_and_serve_directory_handle()?;
    Ok(Task::local(serve_sampler_requests(service_fs, tx)))
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl::endpoints::{Proxy, create_proxy_and_stream};
    use fidl_fuchsia_memory_sampler::{ExecutableSegment, ModuleMap, SamplerMarker, StackTrace};
    use flate2::read::GzDecoder;
    use futures::StreamExt;
    use itertools::{assert_equal, sorted};
    use prost::Message;
    use std::io::Read;
    use zx::{Peered, Vmo};

    use crate::crash_reporter::{MAX_CONCURRENT_PROFILES, ProfileReport, ProfileSender};
    use crate::pprof::pproto::{Location, Mapping, Profile};
    use crate::sampler_service::{
        MAX_DURATION_BETWEEN_PARTIAL_PROFILES, ProfileBuilder,
        RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD, maybe_emit_partial_profile,
        process_sampler_requests, run_sampler_service,
    };

    fn deserialize_profile(profile: Vmo, size: u64) -> Profile {
        let compressed_profile = profile.read_to_vec(0, size).unwrap();
        let mut proto_profile = Vec::new();
        GzDecoder::new(&compressed_profile[..]).read_to_end(&mut proto_profile).unwrap();
        Profile::decode(&proto_profile[..]).unwrap()
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_full_profile() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let module_map = vec![
            ModuleMap {
                build_id: Some(vec![1, 2, 3, 4]),
                executable_segments: Some(vec![
                    ExecutableSegment {
                        start_address: Some(2000),
                        relative_address: Some(0),
                        size: Some(1000),
                        ..Default::default()
                    },
                    ExecutableSegment {
                        start_address: Some(4000),
                        relative_address: Some(2000),
                        size: Some(2000),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
            ModuleMap {
                build_id: Some(vec![3, 4, 5, 6]),
                executable_segments: Some(vec![ExecutableSegment {
                    start_address: Some(3000),
                    relative_address: Some(1000),
                    size: Some(100),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ];
        let allocation_stack_trace =
            StackTrace { stack_frames: Some(vec![1000, 1500]), ..Default::default() };
        let deallocation_stack_trace =
            StackTrace { stack_frames: Some(vec![3000, 3001]), ..Default::default() };

        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("test process".to_string()),
            module_map: Some(module_map),
            ..Default::default()
        })?;
        client.record_allocation(&RecordAllocationEvent {
            address: Some(0x100),
            stack_trace: Some(allocation_stack_trace.clone()),
            size: Some(100),
            ..Default::default()
        })?;
        client.record_allocation(&RecordAllocationEvent {
            address: Some(0x200),
            stack_trace: Some(allocation_stack_trace),
            size: Some(1000),
            ..Default::default()
        })?;
        client.record_deallocation(&RecordDeallocationEvent {
            address: Some(0x100),
            stack_trace: Some(deallocation_stack_trace),
            ..Default::default()
        })?;
        drop(client);

        if let Some(ProfileReport::Final { process_name, size, profile, .. }) =
            profile_future.await?
        {
            assert_eq!("test process", process_name);
            let profile = deserialize_profile(profile, size);
            assert_eq!(3, profile.mapping.len());
            assert_eq!(4, profile.location.len());
            assert_eq!(3, profile.sample.len());
        } else {
            panic!("Expected complete report, got partial report instead.");
        };

        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_request_partial_profile_on_size() -> Result<(), Error> {
        let (_client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, mut rx) = ProfileSender::channel(1);
        const TEST_NAME: &str = "test process";
        let mut builder = ProfileBuilder::default();

        // Pre-fill `builder` with a large number of unique dead
        // allocations, to trigger a partial profile on the next
        // request.
        builder.set_process_info(Some(TEST_NAME.to_string()), vec![].into_iter());
        {
            (0..RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD as u64).for_each(|i| {
                builder.allocate(i, (i..i + 4).collect(), 10, None);
                builder.deallocate(i, (i..i + 4).collect());
            });
        }

        let stack_trace = StackTrace { stack_frames: Some(vec![1000, 1500]), ..Default::default() };
        builder.allocate(
            RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD as u64,
            stack_trace.stack_frames.unwrap(),
            10,
            None,
        );
        let mut budget = StackTraceBudget::default().enter();
        let new_time =
            maybe_emit_partial_profile(&mut builder, &tx, &mut budget, Instant::now(), &None)?;
        assert!(new_time.is_some());
        let _ = request_stream; // Silence unused variable warning
        let report = rx.next().await.unwrap();
        match report {
            ProfileReport::Partial { process_name, profile, size, .. } => {
                assert_eq!(process_name, TEST_NAME.to_string());
                // This test assumes that every single allocation ends
                // up as a sample in the produced profile.
                let profile = deserialize_profile(profile, size);
                assert!(profile.sample.len() > RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD);
            }
            _ => assert!(false),
        };
        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_request_partial_profile_on_time() -> Result<(), Error> {
        let (_client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, mut rx) = ProfileSender::channel(1);
        const TEST_NAME: &str = "test process";
        let mut builder = ProfileBuilder::default();
        builder.set_process_info(Some(TEST_NAME.to_string()), vec![].into_iter());

        let stack_trace = StackTrace { stack_frames: Some(vec![1000, 1500]), ..Default::default() };
        builder.allocate(1, stack_trace.stack_frames.unwrap(), 10, None);
        let mut budget = StackTraceBudget::default().enter();
        let new_time = maybe_emit_partial_profile(
            &mut builder,
            &tx,
            &mut budget,
            Instant::now() - MAX_DURATION_BETWEEN_PARTIAL_PROFILES,
            &None,
        )?;
        assert!(new_time.is_some());
        let _ = request_stream; // Silence unused variable warning
        let report = rx.next().await.unwrap();
        match report {
            ProfileReport::Partial { process_name, profile, size, .. } => {
                assert_eq!(process_name, TEST_NAME.to_string());
                // This test assumes that every single allocation ends
                // up as a sample in the produced profile.
                let profile = deserialize_profile(profile, size);
                assert_eq!(1, profile.sample.len());
            }
            _ => assert!(false),
        };
        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_set_process_info() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let module_map = vec![
            ModuleMap {
                build_id: Some(vec![1, 2, 3, 4]),
                executable_segments: Some(vec![
                    ExecutableSegment {
                        start_address: Some(2000),
                        relative_address: Some(0),
                        size: Some(1000),
                        ..Default::default()
                    },
                    ExecutableSegment {
                        start_address: Some(4000),
                        relative_address: Some(2000),
                        size: Some(2000),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
            ModuleMap {
                build_id: Some(vec![3, 4, 5, 6]),
                executable_segments: Some(vec![ExecutableSegment {
                    start_address: Some(3000),
                    relative_address: Some(1000),
                    size: Some(100),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ];

        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("test process".to_string()),
            module_map: Some(module_map),
            ..Default::default()
        })?;
        drop(client);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!("test process", process_name);
            let profile = deserialize_profile(profile, size);
            assert_eq!(3, profile.mapping.len());
            assert_eq!(Vec::<Location>::new(), profile.location);
        } else {
            panic!("Expected complete report, got partial report instead.");
        };
        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_multiple_set_process_info() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let module_map = vec![
            ModuleMap {
                build_id: Some(vec![1, 2, 3, 4]),
                executable_segments: Some(vec![
                    ExecutableSegment {
                        start_address: Some(2000),
                        relative_address: Some(0),
                        size: Some(1000),
                        ..Default::default()
                    },
                    ExecutableSegment {
                        start_address: Some(4000),
                        relative_address: Some(2000),
                        size: Some(2000),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
            ModuleMap {
                build_id: Some(vec![3, 4, 5, 6]),
                executable_segments: Some(vec![ExecutableSegment {
                    start_address: Some(3000),
                    relative_address: Some(1000),
                    size: Some(100),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        ];

        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("test process".to_string()),
            module_map: Some(module_map.clone()),
            ..Default::default()
        })?;
        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("other test process".to_string()),
            module_map: Some(module_map),
            ..Default::default()
        })?;
        drop(client);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!("other test process", process_name);
            let profile = deserialize_profile(profile, size);
            assert_eq!(6, profile.mapping.len());
            assert_eq!(Vec::<Location>::new(), profile.location);
        } else {
            panic!("Expected complete report, got partial report instead.");
        };

        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_allocate() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let allocation_stack_trace =
            StackTrace { stack_frames: Some(vec![1000, 1500]), ..Default::default() };

        client.record_allocation(&RecordAllocationEvent {
            address: Some(0x100),
            stack_trace: Some(allocation_stack_trace),
            size: Some(100),
            ..Default::default()
        })?;
        drop(client);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!(String::default(), process_name);
            let profile = deserialize_profile(profile, size);
            let locations = profile.location.into_iter().map(|Location { address, .. }| address);
            assert_equal(vec![1000, 1500].into_iter(), sorted(locations));
        } else {
            panic!("Expected complete report, got partial report instead.");
        };
        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_deallocate() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let stack_trace = StackTrace { stack_frames: Some(vec![3000, 3001]), ..Default::default() };

        client.record_deallocation(&RecordDeallocationEvent {
            address: Some(0x100),
            stack_trace: Some(stack_trace),
            ..Default::default()
        })?;
        drop(client);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!(String::default(), process_name);
            let profile = deserialize_profile(profile, size);
            assert_eq!(Vec::<Mapping>::new(), profile.mapping);
            assert_eq!(Vec::<Location>::new(), profile.location);
        } else {
            panic!("Expected complete report, got partial report instead.");
        };

        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_shared_socket() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let (client_sock, server_sock) = zx::Socket::create_datagram();
        client.set_shared_socket(server_sock)?;

        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("socket test process".to_string()),
            module_map: Some(vec![]),
            ..Default::default()
        })?;

        // Send an allocation record over the socket datagram
        let alloc =
            fidl_fuchsia_memory_sampler::SamplerDatagram::RecordAllocation(RecordAllocationEvent {
                address: Some(0x100),
                size: Some(200),
                stack_trace: Some(StackTrace {
                    stack_frames: Some(vec![1000, 1500]),
                    ..Default::default()
                }),
                sampling_interval_bytes: Some(1048576),
                ..Default::default()
            });
        let alloc_bytes = fidl::persist(&alloc)?;
        client_sock.write(&alloc_bytes)?;

        // Send a deallocation record over the socket datagram
        let dealloc = fidl_fuchsia_memory_sampler::SamplerDatagram::RecordDeallocation(
            RecordDeallocationEvent {
                address: Some(0x100),
                stack_trace: Some(StackTrace {
                    stack_frames: Some(vec![3000]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let dealloc_bytes = fidl::persist(&dealloc)?;
        client_sock.write(&dealloc_bytes)?;

        drop(client_sock);
        drop(client);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!("socket test process", process_name);
            let profile = deserialize_profile(profile, size);
            let locations = profile.location.into_iter().map(|Location { address, .. }| address);
            assert_equal(vec![1000, 1500, 3000].into_iter(), sorted(locations));

            // Verify the unsampled math used the provided 1MB rate.
            // P(S) = 1 - e^(-200 / 1048576)
            // scale = 1 / P(S) ≈ 5243.38
            // scale.round() = 5243
            // total_size = (200 * scale) = 1048676.2 => round() = 1048676
            // The sample from dead_allocations (because it was deallocated)
            // has value: [0, 0, count, total_size, 0, 0]
            let alloc_sample =
                profile.sample.iter().find(|s| s.value[2] > 0).expect("Missing allocation sample");
            assert_eq!(alloc_sample.value[2], 5243);
            assert_eq!(alloc_sample.value[3], 1048676);
        } else {
            panic!("Expected complete report, got partial report instead.");
        };

        Ok(())
    }

    #[fuchsia::test]
    async fn test_process_sampler_requests_shared_socket_lossy() -> Result<(), Error> {
        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let profile_future =
            process_sampler_requests(request_stream, &tx, StackTraceBudget::default().enter());

        let (client_sock, server_sock) = zx::Socket::create_datagram();
        client.set_shared_socket(server_sock)?;

        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("socket lossy process".to_string()),
            module_map: Some(vec![]),
            ..Default::default()
        })?;

        // Signal USER_0 from the client socket to indicate buffer saturation
        client_sock.signal_peer(zx::Signals::NONE, zx::Signals::USER_0)?;

        drop(client_sock);
        drop(client);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!("socket lossy process [TAINTED: BUFFER OVERFLOW]", process_name);
            let profile = deserialize_profile(profile, size);
            assert_eq!(1, profile.comment.len());
            let comment = &profile.string_table[profile.comment[0] as usize];
            assert!(comment.contains("WARNING: Socket buffer saturated"));
        } else {
            panic!("Expected complete report, got partial report instead.");
        };

        Ok(())
    }

    #[fuchsia::test]
    async fn test_empty_datagram_is_ignored_and_socket_close_terminates() -> Result<(), Error> {
        assert!(matches!(ClientEvent::from(&[][..]), ClientEvent::Ignored));

        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (tx, _rx) = ProfileSender::channel(1);
        let mut profile_future = Box::pin(process_sampler_requests(
            request_stream,
            &tx,
            StackTraceBudget::default().enter(),
        ));

        let (client_sock, server_sock) = zx::Socket::create_datagram();
        client.set_shared_socket(server_sock)?;
        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("empty datagram test".to_string()),
            module_map: Some(vec![]),
            ..Default::default()
        })?;

        // Poll while the socket is open and empty; the session must remain active.
        assert!(futures::poll!(&mut profile_future).is_pending());

        let alloc =
            fidl_fuchsia_memory_sampler::SamplerDatagram::RecordAllocation(RecordAllocationEvent {
                address: Some(0x200),
                size: Some(64),
                stack_trace: Some(StackTrace {
                    stack_frames: Some(vec![2000]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        client_sock.write(&fidl::persist(&alloc)?)?;

        // Closing the socket peer (even while the FIDL channel remains open) terminates the stream.
        drop(client_sock);

        if let Some(ProfileReport::Final { process_name, profile, size, .. }) =
            profile_future.await?
        {
            assert_eq!("empty datagram test", process_name);
            let profile = deserialize_profile(profile, size);
            let locations = profile.location.into_iter().map(|Location { address, .. }| address);
            assert_equal(vec![2000].into_iter(), sorted(locations));
        } else {
            panic!("Expected complete report, got partial report instead.");
        };

        Ok(())
    }

    #[fuchsia::test]
    async fn test_run_sampler_service_bounds_queued_profiles_under_connection_churn()
    -> Result<(), Error> {
        let (tx, rx) = ProfileSender::channel(MAX_CONCURRENT_PROFILES);
        let churn_connections = MAX_CONCURRENT_PROFILES * 3;

        for i in 0..churn_connections {
            let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
            client.set_process_info(&SamplerSetProcessInfoRequest {
                process_name: Some(format!("churn_proc_{i}")),
                module_map: Some(vec![]),
                ..Default::default()
            })?;
            client.record_allocation(&RecordAllocationEvent {
                address: Some(0x100),
                stack_trace: Some(StackTrace {
                    stack_frames: Some(vec![1000, 1500]),
                    ..Default::default()
                }),
                size: Some(128),
                ..Default::default()
            })?;
            drop(client);

            run_sampler_service(request_stream, tx.clone(), StackTraceBudget::default().enter())
                .await?;
        }
        drop(tx);

        let queued_profiles: Vec<ProfileReport> = rx.collect().await;
        assert_eq!(MAX_CONCURRENT_PROFILES, queued_profiles.len());
        for (i, report) in queued_profiles.into_iter().enumerate() {
            assert_eq!(format!("churn_proc_{i}"), report.process_name());
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn test_run_sampler_service_disconnects_client_when_partial_profile_channel_full()
    -> Result<(), Error> {
        let (tx, mut rx) = ProfileSender::channel(1);
        // Fill the single channel slot so partial profile submission fails with `is_full()`.
        tx.try_send(ProfileReport::Final {
            provider_id: 999,
            process_name: "occupying_proc".to_string(),
            size: 1,
            profile: Vmo::create(1)?,
        })?;

        let (client, request_stream) = create_proxy_and_stream::<SamplerMarker>();
        let (client_sock, server_sock) = zx::Socket::create_datagram();
        client.set_shared_socket(server_sock)?;
        client.set_process_info(&SamplerSetProcessInfoRequest {
            process_name: Some("churn_proc".to_string()),
            module_map: Some(vec![]),
            ..Default::default()
        })?;

        let service_task = Task::local(run_sampler_service(
            request_stream,
            tx.clone(),
            StackTraceBudget::default().enter(),
        ));
        drop(tx);

        // Send unique alloc/dealloc pairs until `RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD`
        // triggers a partial profile while the channel is full. Yield periodically so
        // `service_task` drains the datagram socket instead of saturating its kernel buffer.
        let mut peer_closed = false;
        for i in 0..RECLAIMABLE_STACK_TRACES_PROFILE_THRESHOLD as u64 {
            let stack_trace = StackTrace {
                stack_frames: Some(vec![i + 1, i + 2, i + 3, i + 4]),
                ..Default::default()
            };
            let alloc = fidl_fuchsia_memory_sampler::SamplerDatagram::RecordAllocation(
                RecordAllocationEvent {
                    address: Some(i),
                    size: Some(64),
                    stack_trace: Some(stack_trace.clone()),
                    ..Default::default()
                },
            );
            let dealloc = fidl_fuchsia_memory_sampler::SamplerDatagram::RecordDeallocation(
                RecordDeallocationEvent {
                    address: Some(i),
                    stack_trace: Some(stack_trace),
                    ..Default::default()
                },
            );
            let alloc_bytes = fidl::persist(&alloc)?;
            let dealloc_bytes = fidl::persist(&dealloc)?;

            for bytes in [&alloc_bytes, &dealloc_bytes] {
                loop {
                    match client_sock.write(bytes) {
                        Ok(_) => break,
                        Err(zx::Status::SHOULD_WAIT) => fuchsia_async::yield_now().await,
                        Err(zx::Status::PEER_CLOSED) => {
                            peer_closed = true;
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                if peer_closed {
                    break;
                }
            }
            if peer_closed {
                break;
            }
            if i % 64 == 63 {
                fuchsia_async::yield_now().await;
            }
        }

        // The service task must terminate without waiting for the client to close its handles.
        service_task.await?;

        // Both the datagram socket and the FIDL channel must now observe `PEER_CLOSED`.
        assert_eq!(Err(zx::Status::PEER_CLOSED), client_sock.write(b"after_disconnect"));
        assert!(client.is_closed());

        // Only the pre-filled report is in `rx`; neither the failed partial profile nor a
        // truncated final profile was queued.
        let queued = rx.next().await.expect("Expected pre-filled report");
        assert_eq!("occupying_proc", queued.process_name());
        assert!(rx.next().await.is_none());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_serve_sampler_requests_profiles_many_concurrent_connections_without_starvation()
    -> Result<(), Error> {
        let (tx, mut rx) = ProfileSender::channel(MAX_CONCURRENT_PROFILES);
        let (conn_tx, conn_rx) = futures::channel::mpsc::unbounded();
        let server_task = Task::local(serve_sampler_requests(conn_rx, tx));

        // Open 15 long-lived idle connections first (exceeding the former 10-connection cap).
        let mut idle_clients = Vec::new();
        for i in 0..15 {
            let (client, stream) = create_proxy_and_stream::<SamplerMarker>();
            let (client_sock, server_sock) = zx::Socket::create_datagram();
            client.set_shared_socket(server_sock)?;
            client.set_process_info(&SamplerSetProcessInfoRequest {
                process_name: Some(format!("idle_proc_{i}")),
                module_map: Some(vec![]),
                ..Default::default()
            })?;
            conn_tx.unbounded_send(IncomingServiceRequest::Sampler(stream))?;
            idle_clients.push((client, client_sock));
        }

        // Open 5 active connections after the 15 idle ones; they must be drained and emit
        // their final profiles while all 15 idle connections remain open.
        for i in 0..5 {
            let (client, stream) = create_proxy_and_stream::<SamplerMarker>();
            let (client_sock, server_sock) = zx::Socket::create_datagram();
            client.set_shared_socket(server_sock)?;
            client.set_process_info(&SamplerSetProcessInfoRequest {
                process_name: Some(format!("active_proc_{i}")),
                module_map: Some(vec![]),
                ..Default::default()
            })?;
            conn_tx.unbounded_send(IncomingServiceRequest::Sampler(stream))?;

            let alloc = fidl_fuchsia_memory_sampler::SamplerDatagram::RecordAllocation(
                RecordAllocationEvent {
                    address: Some(0x1000 + i),
                    size: Some(128),
                    stack_trace: Some(StackTrace {
                        stack_frames: Some(vec![0xdead, 0xbeef]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            client_sock.write(&fidl::persist(&alloc)?)?;
            drop(client_sock);
            drop(client);
        }

        let mut active_names = Vec::new();
        for _ in 0..5 {
            let report = rx.next().await.expect("Expected active connection report");
            active_names.push(report.process_name().to_string());
        }
        active_names.sort();
        assert_eq!(
            vec![
                "active_proc_0",
                "active_proc_1",
                "active_proc_2",
                "active_proc_3",
                "active_proc_4",
            ],
            active_names
        );

        drop(idle_clients);
        drop(conn_tx);
        server_task.await?;

        Ok(())
    }

    #[fuchsia::test]
    async fn test_shared_stack_trace_budget_flushes_fair_share_and_ignores_idle_sessions()
    -> Result<(), Error> {
        let shared_budget = StackTraceBudget::default();
        let (tx, mut rx) = ProfileSender::channel(4);

        // 19 contributing peer sessions plus 80 completely idle (0-trace) sessions:
        // idle sessions must not dilute `fair_share` (`30_000 / 20 = 1_500`, NOT `30_000 / 100 = 300`),
        // even when `total_reclaimable >= TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET`.
        let idle_sessions: Vec<SessionStackTraceBudget> =
            (0..80).map(|_| shared_budget.enter()).collect();
        let mut peer_sessions: Vec<SessionStackTraceBudget> =
            (0..19).map(|_| shared_budget.enter()).collect();
        let mut active_session = shared_budget.enter();

        let fair_share = TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET / 20;
        let mut builder = ProfileBuilder::default();
        builder.set_process_info(Some("active_proc".to_string()), std::iter::empty());
        // Record `fair_share - 2` stack traces (`1_498`, well above `30_000 / 100 = 300`
        // but just below `fair_share = 1_500`).
        for i in 0..((fair_share / 2) - 1) as u64 {
            builder.allocate(i, vec![i, i + 1], 32, None);
            builder.deallocate(i, vec![i + 10_000, i + 10_001]);
        }
        assert_eq!(fair_share - 2, builder.get_approximate_reclaimable_stack_traces_count());

        let now = Instant::now();
        maybe_emit_partial_profile(&mut builder, &tx, &mut active_session, now, &None)?;
        assert!(
            rx.try_recv().is_err(),
            "Session below threshold must not flush while global budget has headroom"
        );

        // Push the 19 contributing peers slightly above `fair_share` so global total reaches
        // `TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET` (`30_000`) while `active_session` is still
        // at `fair_share - 2` (`1_498 < 1_500`). Because the 80 0-trace `idle_sessions` are
        // excluded from `contributing_sessions` (`20`), `active_session` still does NOT flush!
        for (idx, peer) in peer_sessions.iter_mut().enumerate() {
            let count = if idx < 2 { fair_share + 1 } else { fair_share };
            let _ = peer.update(count);
        }
        assert_eq!(20, shared_budget.total.get().contributing_sessions);
        assert_eq!(
            TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET,
            shared_budget.total.get().reclaimable_stack_traces
        );
        maybe_emit_partial_profile(&mut builder, &tx, &mut active_session, now, &None)?;
        assert!(
            rx.try_recv().is_err(),
            "0-trace idle sessions must not dilute fair_share under global budget pressure"
        );

        // One more alloc + dealloc brings `active_session` to `fair_share` (`1_500`),
        // triggering a partial profile flush that reclaims its stack traces and decrements
        // `contributing_sessions` from 20 to 19.
        let last = (fair_share / 2) as u64;
        builder.allocate(last, vec![last, last + 1], 32, None);
        builder.deallocate(last, vec![last + 10_000, last + 10_001]);
        assert_eq!(fair_share, builder.get_approximate_reclaimable_stack_traces_count());

        maybe_emit_partial_profile(&mut builder, &tx, &mut active_session, now, &None)?;
        let flushed = rx.next().await.expect("Expected partial profile flush under shared budget");
        assert!(matches!(flushed, ProfileReport::Partial { .. }));
        assert_eq!(0, builder.get_approximate_reclaimable_stack_traces_count());
        assert_eq!(19, shared_budget.total.get().contributing_sessions);
        assert_eq!(
            TOTAL_RECLAIMABLE_STACK_TRACES_BUDGET + 2 - fair_share,
            shared_budget.total.get().reclaimable_stack_traces
        );

        // Dropping sessions releases all remaining tracked counts and contributing session slots.
        drop(active_session);
        drop(peer_sessions);
        drop(idle_sessions);
        assert_eq!(0, shared_budget.total.get().contributing_sessions);
        assert_eq!(0, shared_budget.total.get().reclaimable_stack_traces);

        Ok(())
    }
}
