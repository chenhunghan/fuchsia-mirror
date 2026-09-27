// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::buffer_set::*;
use crate::elementary_stream::*;
use fidl_fuchsia_media::*;
use fuchsia_stream_processors::*;

use std::fmt;
use thiserror::Error;

/// A stream converting elementary stream chunks into input packets for a stream processor.
pub struct InputPacketStream<I> {
    buffer_set: BufferSet,
    stream_lifetime_ordinal: u64,
    stream: I,
    sent_eos: bool,
}

#[derive(Debug, Error)]
pub enum Error {
    PacketRefersToInvalidBuffer,
    BufferTooSmall { buffer_size: usize, stream_chunk_size: usize },
    VmoWriteFail(zx::Status),
}

impl fmt::Display for Error {
    fn fmt(&self, w: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self, w)
    }
}

pub enum PacketPoll {
    Ready(Packet),
    Eos,
    NotReady,
}

impl<'a, I: Iterator<Item = ElementaryStreamChunk>> InputPacketStream<I> {
    pub fn new(buffer_set: BufferSet, stream: I, stream_lifetime_ordinal: u64) -> Self {
        Self { buffer_set, stream_lifetime_ordinal, stream, sent_eos: false }
    }

    pub fn add_free_packet(&mut self, packet: ValidPacketHeader) -> Result<(), Error> {
        let (_, ref mut status) = *self
            .buffer_set
            .packet_and_buffer_pairs
            .get_mut(&packet.packet_index)
            .ok_or(Error::PacketRefersToInvalidBuffer)?;
        *status = crate::buffer_set::UsageStatus::Free;
        Ok(())
    }

    pub fn all_packets_free(&self) -> bool {
        self.buffer_set
            .packet_and_buffer_pairs
            .values()
            .all(|(_, usage)| *usage == crate::buffer_set::UsageStatus::Free)
    }

    fn free_packet_and_buffer(&mut self) -> Option<(PacketIdx, BufferIdx)> {
        // This is a linear search. This may not be appropriate in prod code.
        self.buffer_set.packet_and_buffer_pairs.iter_mut().find_map(|(packet, (buffer, usage))| {
            match usage {
                crate::buffer_set::UsageStatus::Free => {
                    *usage = crate::buffer_set::UsageStatus::InUse;
                    Some((*packet, *buffer))
                }
                crate::buffer_set::UsageStatus::InUse => None,
            }
        })
    }

    fn mark_packet_free(&mut self, packet_idx: PacketIdx) {
        if let Some((_, status)) = self.buffer_set.packet_and_buffer_pairs.get_mut(&packet_idx) {
            *status = crate::buffer_set::UsageStatus::Free;
        }
    }

    pub fn next_packet(&mut self) -> Result<PacketPoll, Error> {
        let (packet_idx, buffer_idx) = if let Some(idxs) = self.free_packet_and_buffer() {
            idxs
        } else {
            return Ok(PacketPoll::NotReady);
        };

        let chunk = if let Some(chunk) = self.stream.next() {
            chunk
        } else if !self.sent_eos {
            self.sent_eos = true;
            self.mark_packet_free(packet_idx);
            return Ok(PacketPoll::Eos);
        } else {
            self.mark_packet_free(packet_idx);
            return Ok(PacketPoll::NotReady);
        };

        let buffer = self
            .buffer_set
            .buffers
            .get(buffer_idx as usize)
            .ok_or(Error::PacketRefersToInvalidBuffer)?;

        if (buffer.size as usize) < chunk.data.len() {
            return Err(Error::BufferTooSmall {
                buffer_size: buffer.size as usize,
                stream_chunk_size: chunk.data.len(),
            });
        }

        buffer.data.write(&chunk.data, 0).map_err(Error::VmoWriteFail)?;

        Ok(PacketPoll::Ready(Packet {
            header: Some(PacketHeader {
                packet_index: Some(packet_idx),
                buffer_lifetime_ordinal: Some(self.buffer_set.buffer_lifetime_ordinal),
                ..Default::default()
            }),
            buffer_index: Some(buffer_idx),
            stream_lifetime_ordinal: Some(self.stream_lifetime_ordinal),
            start_offset: Some(0),
            valid_length_bytes: Some(chunk.data.len() as u32),
            timestamp_ish: chunk.timestamp,
            start_access_unit: Some(chunk.start_access_unit),
            known_end_access_unit: Some(chunk.known_end_access_unit),
            ..Default::default()
        }))
    }

    pub fn take_buffer_set(self) -> BufferSet {
        self.buffer_set
    }
}
