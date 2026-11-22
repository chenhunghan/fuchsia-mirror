// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use attribution_processing::AttributionData;
use attribution_processing::digest::BucketDefinition;
use fidl_fuchsia_memory_attribution_plugin as fplugin;
use fuchsia_trace::duration;
use futures::AsyncWriteExt;
use stalls::MemoryStallMetrics;
use stalls::refaults::PageFaultProvider;
use traces::CATEGORY_MEMORY_CAPTURE;

/// Compressed wire bytes of a [fplugin::Snapshot] ready to be written to a socket.
pub struct AttributionSnapshot {
    compressed_data: Box<[u8]>,
}

impl AttributionSnapshot {
    /// Temporarily moves `attribution_data` into a [fplugin::Snapshot], persists and compresses it,
    /// and reclaims the moved vectors back into [AttributionData] without deep-cloning the
    /// underlying resource or mapping allocations.
    pub fn compress_and_reclaim(
        attribution_data: AttributionData,
        kernel_statistics: fplugin::KernelStatistics,
        memory_stalls: MemoryStallMetrics,
        page_fault_provider: impl PageFaultProvider,
        bucket_definitions: &[BucketDefinition],
    ) -> anyhow::Result<(AttributionSnapshot, AttributionData)> {
        duration!(CATEGORY_MEMORY_CAPTURE, c"compress_and_reclaim:snapshot");
        let AttributionData { principals_vec, resources_vec, resource_names, attributions } =
            attribution_data;

        let mut snapshot = fplugin::Snapshot {
            attributions: Some(attributions.into_iter().map(Into::into).collect()),
            principals: Some(principals_vec.into_iter().map(Into::into).collect()),
            resources: Some(resources_vec.into_iter().map(Into::into).collect()),
            resource_names: Some(resource_names.iter().map(|n| *n.buffer()).collect()),
            kernel_statistics: Some(kernel_statistics),
            performance_metrics: Some(fplugin::PerformanceImpactMetrics {
                some_memory_stalls_ns: memory_stalls.some.as_nanos().try_into().ok(),
                full_memory_stalls_ns: memory_stalls.full.as_nanos().try_into().ok(),
                page_refaults: Some(page_fault_provider.get_refault_count()),
                ..Default::default()
            }),
            bucket_definitions: Some(
                bucket_definitions
                    .iter()
                    .map(|b| fplugin::BucketDefinition {
                        name: Some(b.name.clone()),
                        process: (!b.process.is_empty()).then(|| b.process.clone()),
                        vmo: (!b.vmo.is_empty()).then(|| b.vmo.clone()),
                        principal: (!b.principal.is_empty()).then(|| b.principal.clone()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        };

        let data: Vec<u8> = {
            duration!(CATEGORY_MEMORY_CAPTURE, c"persist:snapshot");
            fidl::persist(&snapshot)?
        };

        // Free fields not needed for `reclaimed`, then compress and drop `data` before
        // reallocating the reclaimed vectors so the uncompressed buffer does not overlap.
        snapshot.resource_names = None;
        snapshot.bucket_definitions = None;
        let compressed = Self::compress_bytes(data)?;

        let reclaimed = AttributionData {
            attributions: snapshot
                .attributions
                .take()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            principals_vec: snapshot
                .principals
                .take()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            resources_vec: snapshot
                .resources
                .take()
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
            resource_names,
        };

        Ok((compressed, reclaimed))
    }

    fn compress_bytes(data: Vec<u8>) -> anyhow::Result<Self> {
        let compressed_data = {
            duration!(
                CATEGORY_MEMORY_CAPTURE,
                c"compress:snapshot",
                "uncompressed" => data.len()
            );
            zstd::compress_sized(&data[..], zstd::DEFAULT_COMPRESSION_LEVEL, data.len() as u64)?
        };
        std::mem::drop(data);
        Ok(Self { compressed_data: compressed_data.into_boxed_slice() })
    }

    pub async fn serve(self, socket: zx::Socket) -> anyhow::Result<()> {
        duration!(
            CATEGORY_MEMORY_CAPTURE,
            c"serve:snapshot",
            "compressed" => self.compressed_data.len()
        );
        fidl::AsyncSocket::from_socket(socket).write_all(&self.compressed_data).await?;
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use attribution_processing::{
        Attribution, GlobalPrincipalIdentifier, Principal, PrincipalDescription, PrincipalType,
        Resource, ResourceReference, ZXName,
    };
    use futures::AsyncReadExt;
    use stalls::MemoryStallMetrics;
    use std::time::Duration;

    #[derive(Clone)]
    struct FakePageFaultProvider;
    impl PageFaultProvider for FakePageFaultProvider {
        fn get_fault_count(&self) -> u64 {
            0
        }
        fn get_refault_count(&self) -> u64 {
            42
        }
    }

    #[fuchsia::test]
    async fn test_serve_socket_gets_closed() {
        let (s1, s2) = zx::Socket::create_stream();
        let (snapshot, _) = AttributionSnapshot::compress_and_reclaim(
            AttributionData {
                principals_vec: Vec::new(),
                resources_vec: Vec::new(),
                resource_names: Vec::new(),
                attributions: Vec::new(),
            },
            fplugin::KernelStatistics::default(),
            MemoryStallMetrics::default(),
            FakePageFaultProvider,
            &[],
        )
        .unwrap();
        let serve_task = fuchsia_async::Task::spawn(snapshot.serve(s1));
        // Close the socket without reading it till the end.
        s2.half_close().unwrap();
        // Verify that the process does not panic and returns an error.
        assert!(serve_task.await.is_err());
    }

    #[fuchsia::test]
    async fn test_compress_and_reclaim_roundtrip() {
        let pid = GlobalPrincipalIdentifier::new_for_test(1);
        let orig_principals = vec![Principal {
            identifier: pid,
            description: Some(PrincipalDescription::Component("core/foo".to_string())),
            principal_type: PrincipalType::Runnable,
            parent: None,
        }];
        let orig_resources = vec![Resource {
            koid: 100,
            name_index: 0,
            resource_type: fplugin::ResourceType::Process(fplugin::Process {
                vmos: Some(vec![200, 201]),
                mappings: Some(vec![fplugin::Mapping {
                    vmo: Some(200),
                    address_base: Some(0x1000),
                    size: Some(0x2000),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        }];
        let orig_names = vec![ZXName::from_string_lossy("foo.cm")];
        let orig_attributions = vec![Attribution {
            source: pid,
            subject: pid,
            resources: vec![ResourceReference::KernelObject(100)],
        }];

        let (compressed, reclaimed) = AttributionSnapshot::compress_and_reclaim(
            AttributionData {
                principals_vec: orig_principals.clone(),
                resources_vec: orig_resources.clone(),
                resource_names: orig_names.clone(),
                attributions: orig_attributions,
            },
            fplugin::KernelStatistics::default(),
            MemoryStallMetrics { some: Duration::from_millis(5), full: Duration::from_millis(2) },
            FakePageFaultProvider,
            &[],
        )
        .expect("compress_and_reclaim should succeed");

        assert_eq!(reclaimed.principals_vec, orig_principals);
        assert_eq!(reclaimed.resources_vec, orig_resources);
        assert_eq!(reclaimed.resource_names, orig_names);
        assert_eq!(reclaimed.attributions.len(), 1);

        let (s1, s2) = zx::Socket::create_stream();
        let write_task = fuchsia_async::Task::spawn(async move {
            compressed.serve(s1).await.unwrap();
        });
        let mut wire = Vec::new();
        fidl::AsyncSocket::from_socket(s2).read_to_end(&mut wire).await.unwrap();
        write_task.await;

        let decoded = zstd::decode_all(std::io::Cursor::new(wire)).unwrap();
        let snap: fplugin::Snapshot = fidl::unpersist(&decoded).unwrap();
        assert_eq!(snap.resources.unwrap().len(), 1);
        assert_eq!(snap.performance_metrics.unwrap().page_refaults, Some(42));
    }
}
