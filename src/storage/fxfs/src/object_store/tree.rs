// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lsm_tree::types::{LayerIterator, MergeableKey, Value};
use crate::lsm_tree::{
    BloomFilterOptions, LSMTree, LockedLayer, Query, Yielder, compact_with_iterator,
};
use crate::object_handle::WriteBytes;
use anyhow::{Context, Error};
use std::future::{Future, ready};

/// If a tree has only one immutable layer larger than this threshold, minor compactions leave it
/// in place rather than merging into it so small flushes don't repeatedly rewrite a large base
/// layer. Kept decoupled from `journal::DEFAULT_RECLAIM_SIZE` so tuning journal reclaim does not
/// silently change compaction base-layer retention.
pub const MINOR_COMPACTION_MERGE_THRESHOLD: u64 = 524_288;

pub trait MajorCompactable<K: 'static, V: 'static> {
    /// Returns an iterator that wraps another iterator that is appropriate for major compactions.
    /// Such an iterator should elide items that don't need to be written if a major compaction is
    /// taking place.
    fn major_iter(
        iter: impl LayerIterator<K, V>,
    ) -> impl Future<Output = Result<impl LayerIterator<K, V>, Error>> + Send {
        ready(Ok(iter))
    }

    /// Returns an iterator that wraps another iterator that is appropriate for minor compactions.
    /// Such an iterator may elide items that don't need to be written given `layers_to_keep`.
    fn minor_iter(
        iter: impl LayerIterator<K, V>,
        _layers_to_keep: impl AsRef<[LockedLayer<K, V>]> + Send + Sync,
    ) -> impl Future<Output = Result<impl LayerIterator<K, V>, Error>> + Send {
        ready(Ok(iter))
    }
}

type Layers<K, V> = Vec<LockedLayer<K, V>>;

/// Picks an appropriate set of layers to flush from the tree and writes them to writer.  After
/// successfully writing a new layer, it returns the layers that should be kept and the old layers
/// that should be purged.  `yielder` will be used to periodically yield.
pub async fn flush<'a, K: MergeableKey, V: Value>(
    tree: &'a LSMTree<K, V>,
    writer: impl WriteBytes + Send,
    yielder: Option<impl Yielder>,
    full_compaction: bool,
) -> Result<(Layers<K, V>, Layers<K, V>), Error>
where
    LSMTree<K, V>: MajorCompactable<K, V>,
{
    let mut layer_set = tree.immutable_layer_set();
    let mut total_size = 0;
    let mut layer_count = 0;
    let mut split_index = if full_compaction {
        layer_set.layers.len()
    } else {
        layer_set
            .layers
            .iter()
            .position(|layer| {
                match layer.handle() {
                    None => {}
                    Some(handle) => {
                        let size = handle.get_size();
                        // Stop adding more layers when the total size of all the immutable layers
                        // so far is less than 3/4 of the layer we are looking at.
                        if total_size > 0 && total_size * 4 < size * 3 {
                            return true;
                        }
                        total_size += size;
                        layer_count += 1;
                    }
                }
                false
            })
            .unwrap_or(layer_set.layers.len())
    };

    // If there's only one immutable layer to merge with and it's big, don't merge with it.
    // Instead, we'll create a new layer and eventually that will grow to be big enough to merge
    // with the existing layers.
    if layer_count == 1
        && total_size > MINOR_COMPACTION_MERGE_THRESHOLD
        && layer_set.layers[split_index - 1].handle().is_some()
    {
        split_index -= 1;
    }

    let layers_to_keep = layer_set.layers.split_off(split_index);

    {
        let start_time = std::time::Instant::now();
        let merged_layer_count = layer_set.layers.len();
        let block_size = writer.block_size();
        let bloom_filter_options = Some(BloomFilterOptions { num_items: layer_set.sum_len() });
        let mut merger = layer_set.merger();
        let iter = merger.query(Query::FullScan).await?;
        let bytes_written = if layers_to_keep.is_empty() {
            let major_iter = LSMTree::<K, V>::major_iter(iter).await?;
            compact_with_iterator(major_iter, bloom_filter_options, writer, block_size, yielder)
                .await
        } else {
            let minor_iter = LSMTree::<K, V>::minor_iter(iter, &layers_to_keep).await?;
            compact_with_iterator(minor_iter, bloom_filter_options, writer, block_size, yielder)
                .await
        }
        .context("ObjectStore::flush")?;

        tree.report_compaction_metrics(bytes_written, start_time.elapsed(), merged_layer_count);
    }

    Ok((layers_to_keep, layer_set.layers))
}
