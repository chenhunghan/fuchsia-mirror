// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::util::{U32_SIZE, convert_log_err};
use bytes::Bytes;
use core::iter::Iterator;
use sparse::reader::SparseReader;
use std::cmp::min;
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::range::Range;
use zerocopy::{FromBytes, Immutable, IntoBytes};

struct BytesBox<T>(Box<[T]>);

impl<T> AsRef<[u8]> for BytesBox<T>
where
    T: IntoBytes,
    [T]: Immutable,
{
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

// Streaming flash is an optional protocol used to reduce bandwidth,
// break up images that are too large for the device download buffer,
// and leverage device async I/O to speed partition flashing.
//
// Terminology used for the data structures in this module:
// * A `StreamCommandList` is a list of commands that describe how to write a
//   partition image to the target partition. These commands are independent and
//   idempotent.
// * A `StreamCommand` is a range within the target partition and a description
//   of what the device should do within that range to generate data to write.
// * A `StreamOp` is a description of the bytes defining the owning `StreamCommand`:
//   either a single repeated 32 bit integer, or arbitrary data with a CRC32 checksum.
//
// `Chunk` and `Payload` correspond to `StreamCommand` and `StreamOp` but are
// used as intermediate structures while processing the partition image into operations.
#[derive(Debug, PartialEq)]
pub enum StreamOp {
    Flash { data: Bytes, crc32: u32 },
    Fill { val: u32, length_bytes: u64 },
}

impl StreamOp {
    fn from_data(data: Bytes) -> Self {
        let crc32 = crc32fast::hash(&data);
        Self::Flash { data, crc32 }
    }

    fn from_fill(val: u32, length_bytes: u64) -> Self {
        Self::Fill { val, length_bytes }
    }
}

#[derive(Debug, PartialEq)]
pub struct StreamCommand {
    pub offset_bytes: u64,
    pub op: StreamOp,
}

impl StreamCommand {
    fn from_fill(val: u32, offset_bytes: u64, length_bytes: u64) -> Self {
        Self { offset_bytes, op: StreamOp::from_fill(val, length_bytes) }
    }

    fn from_data(data: Bytes, offset_bytes: u64) -> Self {
        Self { offset_bytes, op: StreamOp::from_data(data) }
    }

    /// Returns the number of bytes that the command writes to the partition.
    pub fn length_bytes(&self) -> u64 {
        match &self.op {
            // usize is at most 64 bits wide on all supported hosts.
            StreamOp::Flash { data, .. } => data.len() as u64,
            StreamOp::Fill { length_bytes, .. } => *length_bytes,
        }
    }
}

struct Chunk {
    range: Range<u64>,
    payload: Payload,
}

struct ChunkError;

impl TryFrom<sparse::Chunk> for Chunk {
    type Error = ChunkError;

    fn try_from(val: sparse::Chunk) -> Result<Self, Self::Error> {
        match val {
            sparse::Chunk::Raw { start, size } if size > 0 => {
                Ok(Self { range: Range { start, end: start + size }, payload: Payload::Flash })
            }
            sparse::Chunk::Fill { start, size, value } if size > 0 => Ok(Self {
                range: Range { start, end: start + size },
                payload: Payload::Fill(value),
            }),
            _ => Err(ChunkError),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Payload {
    Fill(u32),
    Flash,
}

impl Payload {
    fn from_segment(segment: &[u8]) -> Self {
        let data: &[u32] = if segment.is_empty() {
            &[]
        } else {
            <[u32]>::ref_from_bytes(segment)
                .unwrap_or_else(|_| panic!("Data segment length violation"))
        };
        // If data.iter().next().is_none(),
        // we get a 0-length Fill data with a payload of 0,
        // which is a no-op.
        // Technically we could filter these no-op commands,
        // but even though they are syntactically possible in this context
        // they aren't possible due to the way that chunk_range is constructed.
        let val = data.iter().next().unwrap_or(&0);
        if data.iter().all(|v| v == val) { Self::Fill(*val) } else { Self::Flash }
    }
}

fn range_length<T: std::ops::Sub>(range: Range<T>) -> <T as std::ops::Sub>::Output {
    let Range { start, end } = range;
    end - start
}

impl Chunk {
    fn from_payload(range: Range<u64>, payload: Payload) -> Self {
        Self { range, payload }
    }

    fn from_data(range: Range<u64>, data: &[u8]) -> Self {
        Self::from_payload(range, Payload::from_segment(&data[convert_range::<_, usize>(range)]))
    }

    fn offset(&self) -> u64 {
        self.range.start
    }

    fn len_bytes(&self) -> u64 {
        range_length(self.range)
    }

    fn with_extension(self, length: u64) -> Self {
        let Self { range: Range { start, end }, payload } = self;
        Self { range: Range { start, end: end + length }, payload }
    }
}

fn convert_range<T, U>(r: Range<T>) -> Range<U>
where
    U: TryFrom<T>,
    <U as TryFrom<T>>::Error: std::error::Error,
{
    Range { start: convert_log_err(r.start).unwrap(), end: convert_log_err(r.end).unwrap() }
}

// Process the segment into the work-in-progress chunk.
// The current chunk is either expanded to include the new segment OR
// is wrapped up into a completed command, pushed to the command vector,
// and a new work-in-progress chunk is returned.
fn process_segment(
    data: &Bytes,
    segment_range: Range<u64>,
    max_download_bytes: u64,
    commands: &mut Vec<StreamCommand>,
    chunk: Chunk,
) -> Chunk {
    use Payload::*;

    let segment_len_bytes = range_length(segment_range);
    let new_payload =
        Payload::from_segment(&data.as_ref()[convert_range::<_, usize>(segment_range)]);
    match (chunk.payload, new_payload) {
        // New payload is a flash or a fill with a different value
        (p1 @ Fill(val), p2) if p1 != p2 => {
            commands.push(StreamCommand::from_fill(val, chunk.offset(), chunk.len_bytes()));
            Chunk::from_payload(segment_range, new_payload)
        }
        (Flash, Fill(_)) => {
            commands.push(StreamCommand::from_data(
                data.slice(convert_range::<_, usize>(chunk.range)),
                chunk.offset(),
            ));
            Chunk::from_payload(segment_range, new_payload)
        }
        (Flash, Flash)
            if chunk.len_bytes().checked_add(segment_len_bytes).expect("Integer overflow")
                > max_download_bytes =>
        {
            // Can't have a Flash segment longer than max_download_bytes.
            // Even though the following chunk is also raw data, we need to
            // split it up into multiple commands.
            commands.push(StreamCommand::from_data(
                data.slice(convert_range::<_, usize>(chunk.range)),
                chunk.offset(),
            ));
            Chunk::from_payload(segment_range, new_payload)
        }
        // Matching fill or under max size flash
        _ => chunk.with_extension(segment_len_bytes),
    }
}

// Sparse images are enough similar to streaming commands to be converted
// on the fly by the host.
//
// For the most part sparse images can be converted directly to a sequence of
// StreamCommand structs, but there's a catch: sparse files can be very large.
// To prevent excessive memory usage/OoM on the host, we construct an iterator
// that dynamically generates and yields StreamCommands from a sparse image, never
// allocating a buffer larger than the max download size at once.
pub struct SparseStreamIterator<R> {
    // The size of the image written to the target partition.
    expanded_size: u64,
    reader: R,
    chunks: std::vec::IntoIter<(Chunk, Option<u64>)>,
    // Used for splitting sparse raw chunks.
    // If Some, stores a tuple of bytes remaining in the chunk
    // and the offset in the expanded image.
    remaining_bytes_and_offset: Option<(NonZeroU64, u64)>,
    max_download_bytes: u64,
}

impl<R: Read + Seek> SparseStreamIterator<R> {
    pub fn new(max_download_bytes: u64, sparse_reader: SparseReader<R>) -> Self {
        let (reader, _, expanded_size, chunks, _) = sparse_reader.destruct();
        let mut merged_chunks: Vec<(Chunk, Option<u64>)> = vec![];
        for (chunk, idx) in chunks
            .into_iter()
            .filter_map(|(chunk, idx)| Chunk::try_from(chunk).ok().map(|c: Chunk| (c, idx)))
        {
            // Sparse image tools split long fills into several chunks, e.g. libsparse at 64 MiB.
            // A fill sends no data, so merge them to save commands.
            if let Some((last, _)) = merged_chunks.last_mut()
                && let (Payload::Fill(last_val), Payload::Fill(val)) = (last.payload, chunk.payload)
                && last_val == val
                && last.range.end == chunk.range.start
            {
                last.range.end = chunk.range.end;
                continue;
            }
            merged_chunks.push((chunk, idx));
        }
        Self {
            expanded_size,
            reader,
            chunks: merged_chunks.into_iter(),
            remaining_bytes_and_offset: None,
            max_download_bytes,
        }
    }

    pub fn get_expanded_size(&self) -> u64 {
        self.expanded_size
    }

    // Generate a stream flash command to handle a max_download_bytes sized chunk of
    // the current Flash segment or the remainder of the segment, whichever is smaller.
    // Stores metadata if there is any remaining data that needs to be handled.
    //
    // It is the caller's responsibility to call self.reader.seek() at the start of a
    // new Flash command. The reader cursor is preserved across calls,
    // so handling consecutive subchunks works out fine.
    fn handle_subchunk(
        &mut self,
        bytes_remaining: u64,
        offset: u64,
    ) -> std::io::Result<StreamCommand> {
        let buf_len = min(self.max_download_bytes, bytes_remaining);
        let mut buf = vec![0u8; usize::try_from(buf_len).map_err(std::io::Error::other)?];
        self.reader.read_exact(&mut buf)?;

        self.remaining_bytes_and_offset =
            NonZeroU64::new(bytes_remaining - buf_len).map(|s| (s, offset + buf_len));

        Ok(StreamCommand::from_data(Bytes::from_owner(buf.into_boxed_slice()), offset))
    }
}

impl<R: Read + Seek> Iterator for SparseStreamIterator<R> {
    // Reading the image may fail, e.g. if it is truncated.
    type Item = std::io::Result<StreamCommand>;

    // We need to handle exactly one case specially:
    // if a Raw chunk is larger than the max download size, then it gets split.
    // Fills are converted directly (adjacent fills of the same value are merged), CRC and DONTCARE
    // are filtered, and it's not a problem if we generate Flash commands that are smaller than
    // max_download_bytes. It's more trouble than it's worth to merge Flash commands.
    fn next(&mut self) -> Option<Self::Item> {
        let cmd = if let Some((remainder, offset)) = self.remaining_bytes_and_offset.take() {
            self.handle_subchunk(remainder.into(), offset)
        } else {
            let (chunk, reader_offset) = self.chunks.next()?;
            match chunk.payload {
                Payload::Fill(val) => {
                    Ok(StreamCommand::from_fill(val, chunk.offset(), chunk.len_bytes()))
                }
                Payload::Flash => match reader_offset {
                    Some(reader_offset) => self
                        .reader
                        .seek(SeekFrom::Start(reader_offset))
                        .and_then(|_| self.handle_subchunk(chunk.len_bytes(), chunk.offset())),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "raw sparse chunk without data offset",
                    )),
                },
            }
        };
        if cmd.is_err() {
            // Stops after an error.
            self.chunks = vec![].into_iter();
            self.remaining_bytes_and_offset = None;
        }
        Some(cmd)
    }
}

/// Default read buffer size (1 MiB) for scanning raw partition images.
const RAW_READ_BUF_BYTES: usize = 1024 * 1024;
/// Minimum initial allocation (64 KiB in 32-bit words) when accumulating a `Flash` command.
const INITIAL_FLASH_CAP_WORDS: usize = 16 * 1024;

enum PendingRawOp {
    Fill { val: u32, offset_bytes: u64, length_bytes: u64 },
    Flash { offset_bytes: u64, words: Vec<u32> },
}

impl PendingRawOp {
    fn into_command(self) -> StreamCommand {
        match self {
            Self::Fill { val, offset_bytes, length_bytes } => {
                StreamCommand::from_fill(val, offset_bytes, length_bytes)
            }
            Self::Flash { offset_bytes, words } => StreamCommand::from_data(
                Bytes::from_owner(BytesBox(words.into_boxed_slice())),
                offset_bytes,
            ),
        }
    }
}

fn push_flash_words(words: &mut Vec<u32>, seg: &[u32], max_words: usize) {
    let needed = words.len() + seg.len();
    if needed > words.capacity() {
        let desired =
            (words.capacity().max(INITIAL_FLASH_CAP_WORDS / 2) * 2).max(needed).min(max_words);
        words.reserve_exact(desired - words.len());
    }
    words.extend_from_slice(seg);
}

fn new_flash_words(seg: &[u32], max_words: usize) -> Vec<u32> {
    let mut words = Vec::new();
    push_flash_words(&mut words, seg, max_words);
    words
}

/// Incrementally generates [`StreamCommand`]s from a raw (non-sparse) partition image reader,
/// bounding memory use to at most one `max_download_bytes` command buffer plus a small read buffer.
pub struct RawStreamIterator<R> {
    reader: R,
    image_size_bytes: u64,
    offset_bytes: u64,
    max_download_bytes: u64,
    max_download_words: usize,
    segment_size_bytes: u64,
    first_segment_bytes: u64,
    io_buf: Vec<u32>,
    io_pos_words: usize,
    io_len_words: usize,
    pending: Option<PendingRawOp>,
    done: bool,
}

impl<R: Read> RawStreamIterator<R> {
    pub fn new(
        reader: R,
        image_size_bytes: u64,
        max_download_bytes: u64,
        segment_size_bytes: u64,
        partition_start_byte: u64,
    ) -> std::io::Result<Self> {
        if image_size_bytes % U32_SIZE != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("raw image size {image_size_bytes} is not a multiple of {U32_SIZE}"),
            ));
        }
        if segment_size_bytes == 0 || segment_size_bytes % U32_SIZE != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("segment size {segment_size_bytes} is invalid"),
            ));
        }
        if max_download_bytes < segment_size_bytes || max_download_bytes % U32_SIZE != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("max download size {max_download_bytes} is invalid"),
            ));
        }
        if partition_start_byte % U32_SIZE != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("partition start {partition_start_byte} is not a multiple of {U32_SIZE}"),
            ));
        }

        let segment_words =
            usize::try_from(segment_size_bytes / U32_SIZE).map_err(std::io::Error::other)?;
        let max_download_words =
            usize::try_from(max_download_bytes / U32_SIZE).map_err(std::io::Error::other)?;
        let rem = partition_start_byte % segment_size_bytes;
        let first_segment_bytes =
            if rem == 0 { segment_size_bytes } else { segment_size_bytes - rem };

        let target_io_words = (RAW_READ_BUF_BYTES / (U32_SIZE as usize)).max(segment_words);
        let io_words = match usize::try_from(image_size_bytes / U32_SIZE) {
            Ok(total_words) => total_words.min(target_io_words),
            Err(_) => target_io_words,
        };

        Ok(Self {
            reader,
            image_size_bytes,
            offset_bytes: 0,
            max_download_bytes,
            max_download_words,
            segment_size_bytes,
            first_segment_bytes,
            io_buf: vec![0u32; io_words],
            io_pos_words: 0,
            io_len_words: 0,
            pending: None,
            done: image_size_bytes == 0,
        })
    }

    fn ensure_buffered_words(&mut self, seg_words: usize) -> std::io::Result<()> {
        let avail = self.io_len_words - self.io_pos_words;
        if avail < seg_words {
            if avail > 0 {
                self.io_buf.copy_within(self.io_pos_words..self.io_len_words, 0);
            }
            self.io_pos_words = 0;
            self.io_len_words = avail;
            let unread_words_u64 =
                (self.image_size_bytes - self.offset_bytes) / U32_SIZE - (avail as u64);
            let cap_free = self.io_buf.len() - avail;
            let to_read_words = match usize::try_from(unread_words_u64) {
                Ok(u) => u.min(cap_free),
                Err(_) => cap_free,
            };
            self.reader.read_exact(self.io_buf[avail..avail + to_read_words].as_mut_bytes())?;
            self.io_len_words = avail + to_read_words;
        }
        Ok(())
    }

    fn fail(&mut self, err: std::io::Error) -> std::io::Result<StreamCommand> {
        self.done = true;
        self.pending = None;
        self.io_buf = Vec::new();
        Err(err)
    }
}

impl<R: Read> Iterator for RawStreamIterator<R> {
    type Item = std::io::Result<StreamCommand>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        while self.offset_bytes < self.image_size_bytes {
            let remaining = self.image_size_bytes - self.offset_bytes;
            let target_seg_bytes = if self.offset_bytes == 0 {
                self.first_segment_bytes
            } else {
                self.segment_size_bytes
            };
            let seg_len_bytes = min(target_seg_bytes, remaining);
            let seg_words_len = match usize::try_from(seg_len_bytes / U32_SIZE) {
                Ok(n) => n,
                Err(e) => return Some(self.fail(std::io::Error::other(e))),
            };

            if let Err(e) = self.ensure_buffered_words(seg_words_len) {
                return Some(self.fail(e));
            }

            let seg_offset = self.offset_bytes;
            let seg_words = &self.io_buf[self.io_pos_words..self.io_pos_words + seg_words_len];
            self.io_pos_words += seg_words_len;
            self.offset_bytes += seg_len_bytes;

            let first_word = seg_words.first().copied().unwrap_or(0);
            let new_payload = if seg_words.iter().all(|&w| w == first_word) {
                Payload::Fill(first_word)
            } else {
                Payload::Flash
            };

            match (self.pending.take(), new_payload) {
                (None, Payload::Fill(val)) => {
                    self.pending = Some(PendingRawOp::Fill {
                        val,
                        offset_bytes: seg_offset,
                        length_bytes: seg_len_bytes,
                    });
                }
                (None, Payload::Flash) => {
                    self.pending = Some(PendingRawOp::Flash {
                        offset_bytes: seg_offset,
                        words: new_flash_words(seg_words, self.max_download_words),
                    });
                }
                (
                    Some(PendingRawOp::Fill { val: v1, offset_bytes, length_bytes }),
                    Payload::Fill(v2),
                ) if v1 == v2 => {
                    self.pending = Some(PendingRawOp::Fill {
                        val: v1,
                        offset_bytes,
                        length_bytes: length_bytes + seg_len_bytes,
                    });
                }
                (Some(prev @ PendingRawOp::Fill { .. }), Payload::Fill(val)) => {
                    self.pending = Some(PendingRawOp::Fill {
                        val,
                        offset_bytes: seg_offset,
                        length_bytes: seg_len_bytes,
                    });
                    return Some(Ok(prev.into_command()));
                }
                (Some(prev @ PendingRawOp::Fill { .. }), Payload::Flash) => {
                    self.pending = Some(PendingRawOp::Flash {
                        offset_bytes: seg_offset,
                        words: new_flash_words(seg_words, self.max_download_words),
                    });
                    return Some(Ok(prev.into_command()));
                }
                (Some(prev @ PendingRawOp::Flash { .. }), Payload::Fill(val)) => {
                    self.pending = Some(PendingRawOp::Fill {
                        val,
                        offset_bytes: seg_offset,
                        length_bytes: seg_len_bytes,
                    });
                    return Some(Ok(prev.into_command()));
                }
                (Some(PendingRawOp::Flash { offset_bytes, mut words }), Payload::Flash) => {
                    let cur_bytes = (words.len() as u64) * U32_SIZE;
                    if cur_bytes.checked_add(seg_len_bytes).expect("Integer overflow")
                        > self.max_download_bytes
                    {
                        let emitted = PendingRawOp::Flash { offset_bytes, words }.into_command();
                        self.pending = Some(PendingRawOp::Flash {
                            offset_bytes: seg_offset,
                            words: new_flash_words(seg_words, self.max_download_words),
                        });
                        return Some(Ok(emitted));
                    }
                    push_flash_words(&mut words, seg_words, self.max_download_words);
                    self.pending = Some(PendingRawOp::Flash { offset_bytes, words });
                }
            }
        }

        self.done = true;
        self.io_buf = Vec::new();
        self.pending.take().map(|op| Ok(op.into_command()))
    }
}

/// Generate a list of streaming flash commands.
/// Takes a partition image, the max download size the device supports,
/// the streaming segment size, and the partition offset from the start of the device.
///
/// Note: `data` is a boxed slice of u32, but all other parameters are denominated in bytes.
/// The passed data is denominated in u32 to allow aligned value checking for making
/// Fill commands, which describe a repeated 32-bit integer.
/// Other operations on the data treats it as a byte array due to limitations of the
/// Bytes structure.
pub fn generate_command_list(
    data: Box<[u32]>,
    max_download_bytes: u64,
    segment_size_bytes: u64,
    partition_start_byte: u64,
) -> Vec<StreamCommand> {
    if data.is_empty() {
        return vec![];
    }
    let data = Bytes::from_owner(BytesBox(data));
    let prefix_bytes =
        partition_start_byte.next_multiple_of(segment_size_bytes) - partition_start_byte;

    let data_len: u64 = convert_log_err(data.len()).unwrap();
    let prefix_bytes = if prefix_bytes == 0 { segment_size_bytes } else { prefix_bytes };
    // The first chunk primes the process_segment sequence
    // and handles a possible unaligned prefix.
    let mut chunk = Chunk::from_data(Range { start: 0, end: min(prefix_bytes, data_len) }, &data);

    let mut commands = vec![];
    for segment_range in (prefix_bytes..data_len)
        .step_by(convert_log_err(segment_size_bytes).unwrap())
        .map(|start| Range { start, end: start + min(segment_size_bytes, data_len - start) })
    {
        chunk = process_segment(&data, segment_range, max_download_bytes, &mut commands, chunk);
    }

    let final_command = match chunk.payload {
        Payload::Fill(val) => StreamCommand::from_fill(val, chunk.offset(), chunk.len_bytes()),
        Payload::Flash => StreamCommand::from_data(
            data.slice(convert_range::<_, usize>(chunk.range)),
            chunk.offset(),
        ),
    };
    commands.push(final_command);

    commands
}

#[cfg(test)]
mod test {
    use core::ops::{Deref, DerefMut};
    use std::io::Write;

    use sparse::builder::{DataSource, SparseImageBuilder};

    use super::*;
    use crate::util::{U32_SIZE, multi_chain};

    #[fuchsia::test]
    fn test_payload_unaligned_empty_segment() {
        let data = Bytes::from_owner(BytesBox(vec![0u32].into_boxed_slice()));
        let empty = data.slice(1..1);
        assert!(empty.is_empty());
        assert!(!empty.as_ptr().cast::<u32>().is_aligned());
        assert!(matches!(Payload::from_segment(empty.as_ref()), Payload::Fill(0)));
    }

    #[fuchsia::test]
    fn test_payload_word_patterns() {
        for (words, expected) in [
            (vec![0u32], Payload::Fill(0)),
            (vec![0x1234_5678; 3], Payload::Fill(0x1234_5678)),
            (vec![u32::MAX; 2], Payload::Fill(u32::MAX)),
            (vec![1, 2], Payload::Flash),
        ] {
            let data = Bytes::from_owner(BytesBox(words.into_boxed_slice()));
            assert!(Payload::from_segment(data.as_ref()) == expected);
        }
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_basic() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;
        let flash_data = 0u32..SEGMENT_SIZE_WORDS;
        let fill_data = std::iter::repeat(0u32).take(SEGMENT_SIZE_WORDS as usize);
        let data = multi_chain!(
            flash_data.clone(),
            fill_data.clone(),
            fill_data.clone(),
            fill_data.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>()
        .into_boxed_slice();

        let command_list =
            generate_command_list(data, 1 << 32, u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE, 0);

        let cmd_data =
            Bytes::from_owner(BytesBox(Vec::from_iter(flash_data.clone()).into_boxed_slice()));
        assert_eq!(
            command_list,
            [
                StreamCommand::from_data(cmd_data.clone(), 0),
                StreamCommand::from_fill(0, 4096, 3 * 4096),
                StreamCommand::from_data(cmd_data.clone(), 16384),
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_different_fill() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;
        let fill_zero = std::iter::repeat(0u32).take(SEGMENT_SIZE_WORDS as usize);
        let fill_one = std::iter::repeat(1u32).take(SEGMENT_SIZE_WORDS as usize);

        let data = multi_chain!(
            fill_zero.clone(),
            fill_zero.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_zero.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_one.clone(),
            fill_zero.clone(),
            fill_zero.clone(),
        )
        .collect::<Vec<_>>()
        .into_boxed_slice();

        let command_list =
            generate_command_list(data, 1 << 32, u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE, 0);

        assert_eq!(
            command_list,
            [
                StreamCommand::from_fill(0, 0, 8192),
                StreamCommand::from_fill(1, 8192, 8192),
                StreamCommand::from_fill(0, 16384, 4096),
                StreamCommand::from_fill(1, 20480, 16384),
                StreamCommand::from_fill(0, 36864, 8192),
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_split_flash() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let flash_data = 0..SEGMENT_SIZE_WORDS;
        let data = multi_chain!(
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>()
        .into_boxed_slice();

        let command_list = generate_command_list(
            data,
            u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE * 2,
            u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE,
            0,
        );

        let command_data = Bytes::from_owner(BytesBox(
            multi_chain!(flash_data.clone(), flash_data.clone())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ));
        assert_eq!(
            command_list,
            [
                StreamCommand::from_data(command_data.clone(), 0),
                StreamCommand::from_data(command_data.clone(), 8192),
                StreamCommand::from_data(command_data.clone(), 16384),
                StreamCommand::from_data(command_data.clone(), 24576),
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_unaligned_prefix() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let flash_data = 0..SEGMENT_SIZE_WORDS;
        let data = multi_chain!(
            0..(SEGMENT_SIZE_WORDS / 2),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>();

        let command_list = generate_command_list(
            data.clone().into_boxed_slice(),
            1 << 32,
            u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE,
            u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE / 2,
        );

        assert_eq!(
            command_list,
            [StreamCommand::from_data(
                Bytes::from_owner(BytesBox(data.clone().into_boxed_slice())),
                0
            )]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_trailing_suffix() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let fill_zero = std::iter::repeat(0u32).take(SEGMENT_SIZE_WORDS as usize);
        let flash_data = 0..(SEGMENT_SIZE_WORDS / 2);
        let data = multi_chain!(
            fill_zero.clone(),
            fill_zero.clone(),
            fill_zero.clone(),
            fill_zero.clone(),
            flash_data.clone(),
        )
        .collect::<Vec<_>>()
        .into_boxed_slice();

        let command_list =
            generate_command_list(data, 1 << 32, u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE, 0);

        assert_eq!(
            command_list,
            [
                StreamCommand::from_fill(0, 0, 16384),
                StreamCommand::from_data(
                    Bytes::from_owner(BytesBox(flash_data.collect::<Vec<_>>().into_boxed_slice())),
                    16384
                )
            ]
        );
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_tiny_image() {
        const SEGMENT_SIZE_WORDS: u32 = 1024;

        let data = vec![0u32; 8].into_boxed_slice();
        let command_list =
            generate_command_list(data, 1 << 32, u64::from(SEGMENT_SIZE_WORDS) * U32_SIZE, 0);

        assert_eq!(command_list, [StreamCommand::from_fill(0, 0, 32)]);
    }

    struct MemFile<B> {
        data: B,
        cursor: usize,
    }

    impl<B> MemFile<B> {
        fn new(data: B) -> Self {
            Self { data, cursor: 0 }
        }
    }

    impl<B: Deref<Target = [u8]>> Seek for MemFile<B> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let new_cursor = match pos {
                SeekFrom::End(v) => {
                    i64::try_from(self.data.len()).map(|e| e + v).and_then(u64::try_from).unwrap()
                }
                SeekFrom::Start(v) => v,
                SeekFrom::Current(v) => {
                    i64::try_from(self.cursor).map(|c| c + v).and_then(u64::try_from).unwrap()
                }
            };
            self.cursor = min(new_cursor.try_into().unwrap(), self.data.len());
            Ok(self.cursor.try_into().unwrap())
        }
    }

    impl<B: Deref<Target = [u8]>> Read for MemFile<B> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(remaining_data) = self.data.len().checked_sub(self.cursor) {
                let data_length = min(buf.len(), remaining_data);
                (&mut buf[..data_length])
                    .copy_from_slice(&self.data[self.cursor..self.cursor + data_length]);
                self.cursor += data_length;
                Ok(data_length)
            } else {
                Ok(0)
            }
        }
    }

    impl<B: DerefMut<Target = [u8]>> Write for MemFile<B> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(remaining_buffer) = self.data.len().checked_sub(self.cursor) {
                let data_length = min(buf.len(), remaining_buffer);
                (&mut self.data[self.cursor..self.cursor + data_length])
                    .copy_from_slice(&buf[..data_length]);
                self.cursor += data_length;
                Ok(data_length)
            } else {
                Ok(0)
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[fuchsia::test()]
    fn test_stream_command_generation_sparse_basic() {
        let raw = (0u8..255).cycle().take(4096);
        let data = multi_chain!(raw.clone(), raw.clone(), raw.clone(), raw.clone())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let short_data = raw.clone().collect::<Vec<_>>().into_boxed_slice();

        let builder = SparseImageBuilder::new()
            // Basic raw data
            .add_source(DataSource::Buffer(data.clone()))
            // Skip/DONTCARE gets filtered
            .add_source(DataSource::Skip(16384))
            // Basic fill
            .add_source(DataSource::Fill(0, 4096))
            // Second basic raw data
            .add_source(DataSource::Buffer(data.clone()))
            // Spacer
            .add_source(DataSource::Fill(0, 4096))
            // Raw data smaller than max download
            .add_source(DataSource::Buffer(short_data.clone()));
        let mut sparse_image_file =
            MemFile::new(vec![0u8; builder.built_size().try_into().unwrap()]);
        builder.build(&mut sparse_image_file).unwrap();
        sparse_image_file.seek(SeekFrom::Start(0)).unwrap();

        let sparse_reader = SparseReader::new(sparse_image_file).unwrap();
        let commands = SparseStreamIterator::new(8192, sparse_reader)
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();

        let data_helper =
            Bytes::from_owner(data[0..8192].iter().copied().collect::<Vec<_>>().into_boxed_slice());
        assert_eq!(
            commands,
            [
                StreamCommand::from_data(data_helper.clone(), 0),
                StreamCommand::from_data(data_helper.clone(), 8192),
                StreamCommand::from_fill(0, 32768, 16384),
                StreamCommand::from_data(data_helper.clone(), 49152),
                StreamCommand::from_data(data_helper.clone(), 57344),
                StreamCommand::from_fill(0, 65536, 16384),
                StreamCommand::from_data(Bytes::from_owner(short_data), 81920),
            ]
        );
    }

    #[fuchsia::test]
    fn test_stream_command_generation_empty_image() {
        let command_list = generate_command_list(Box::new([]), 1 << 32, 4096, 0);
        assert!(command_list.is_empty());
    }

    /// Builds a sparse image from `sources` and returns a reader for it.
    fn sparse_image(sources: Vec<DataSource>) -> SparseReader<MemFile<Vec<u8>>> {
        let builder = sources.into_iter().fold(SparseImageBuilder::new(), |b, s| b.add_source(s));
        let mut file = MemFile::new(vec![0u8; builder.built_size().try_into().unwrap()]);
        builder.build(&mut file).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        SparseReader::new(file).unwrap()
    }

    #[fuchsia::test]
    fn test_stream_command_generation_sparse_merges_adjacent_fills() {
        let data = (0u8..255).cycle().take(4096).collect::<Vec<_>>().into_boxed_slice();
        let sparse_reader = sparse_image(vec![
            DataSource::Buffer(data.clone()),
            // Adjacent fills of the same value become one command.
            DataSource::Fill(0, 1024),
            DataSource::Fill(0, 2048),
            // A fill of another value does not merge.
            DataSource::Fill(7, 1024),
            // Fills separated by data or skipped ranges do not merge.
            DataSource::Skip(4096),
            DataSource::Fill(7, 1024),
            DataSource::Buffer(data.clone()),
            DataSource::Fill(7, 1024),
        ]);
        let commands = SparseStreamIterator::new(1 << 20, sparse_reader)
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();

        let data = Bytes::from_owner(data);
        assert_eq!(
            commands,
            [
                StreamCommand::from_data(data.clone(), 0),
                StreamCommand::from_fill(0, 4096, 12288),
                StreamCommand::from_fill(7, 16384, 4096),
                StreamCommand::from_fill(7, 24576, 4096),
                StreamCommand::from_data(data.clone(), 28672),
                StreamCommand::from_fill(7, 32768, 4096),
            ]
        );
        assert_eq!(
            commands.iter().map(StreamCommand::length_bytes).collect::<Vec<_>>(),
            [4096, 12288, 4096, 4096, 4096, 4096]
        );
    }

    #[fuchsia::test]
    fn test_stream_command_generation_sparse_truncated_image() {
        let data = (0u8..255).cycle().take(8192).collect::<Vec<_>>().into_boxed_slice();
        let builder = SparseImageBuilder::new().add_source(DataSource::Buffer(data));
        let mut image = vec![0u8; builder.built_size().try_into().unwrap()];
        builder.build(&mut MemFile::new(&mut image[..])).unwrap();
        image.truncate(image.len() - 100);
        let sparse_reader = SparseReader::new(MemFile::new(image)).unwrap();

        let mut commands = SparseStreamIterator::new(1 << 20, sparse_reader);
        assert!(commands.next().unwrap().is_err(), "the truncated data fails to read");
        assert!(commands.next().is_none(), "no commands after an error");
    }

    #[fuchsia::test]
    fn test_stream_command_generation_sparse_truncated_subchunk_stops_before_trailing_chunks() {
        struct LimitedReader<R> {
            inner: R,
            remaining_bytes: std::rc::Rc<std::cell::Cell<usize>>,
        }

        impl<R: Read> Read for LimitedReader<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let rem = self.remaining_bytes.get();
                if rem == 0 {
                    return Ok(0);
                }
                let max = buf.len().min(rem);
                let n = self.inner.read(&mut buf[..max])?;
                self.remaining_bytes.set(rem - n);
                Ok(n)
            }
        }

        impl<R: Seek> Seek for LimitedReader<R> {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(pos)
            }
        }

        let data = (0u8..255).cycle().take(8192).collect::<Vec<_>>().into_boxed_slice();
        let builder = SparseImageBuilder::new()
            .add_source(DataSource::Buffer(data))
            .add_source(DataSource::Fill(7, 1024));
        let mut image = vec![0u8; builder.built_size().try_into().unwrap()];
        builder.build(&mut MemFile::new(&mut image[..])).unwrap();

        let remaining_bytes = std::rc::Rc::new(std::cell::Cell::new(usize::MAX));
        let sparse_reader = SparseReader::new(LimitedReader {
            inner: MemFile::new(image),
            remaining_bytes: remaining_bytes.clone(),
        })
        .unwrap();

        // Allow the first 4096-byte subchunk to succeed, then hit EOF 100 bytes into the second.
        remaining_bytes.set(4096 + 100);
        let mut commands = SparseStreamIterator::new(4096, sparse_reader);
        assert!(commands.next().unwrap().is_ok(), "first 4096-byte subchunk succeeds");
        assert!(commands.next().unwrap().is_err(), "truncated second subchunk fails");
        assert!(
            commands.next().is_none(),
            "iterator must fuse after a subchunk error rather than yielding trailing chunks"
        );
    }

    #[fuchsia::test]
    fn test_sparse_zero_size_chunk_filtered() {
        assert!(Chunk::try_from(sparse::Chunk::Raw { start: 0, size: 0 }).is_err());
        assert!(Chunk::try_from(sparse::Chunk::Fill { start: 0, size: 0, value: 0 }).is_err());
    }

    fn assert_raw_stream_matches_generate(
        words: &[u32],
        max_download_bytes: u64,
        segment_size_bytes: u64,
        partition_start_byte: u64,
    ) {
        let expected = generate_command_list(
            words.to_vec().into_boxed_slice(),
            max_download_bytes,
            segment_size_bytes,
            partition_start_byte,
        );
        let bytes = words.as_bytes();
        let actual = RawStreamIterator::new(
            MemFile::new(bytes),
            bytes.len() as u64,
            max_download_bytes,
            segment_size_bytes,
            partition_start_byte,
        )
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[fuchsia::test]
    fn test_raw_stream_iterator_parity_across_windows_and_fills() {
        const SEG_WORDS: usize = 256; // 1 KiB segment
        let seg_bytes = (SEG_WORDS as u64) * U32_SIZE;
        let flash_a = (0..SEG_WORDS as u32).map(|i| i.wrapping_mul(31).wrapping_add(7));
        let flash_b = (0..SEG_WORDS as u32).map(|i| i ^ 0xdead_beef);
        let fill_zero = std::iter::repeat_n(0u32, SEG_WORDS * 5);
        let fill_ones = std::iter::repeat_n(u32::MAX, SEG_WORDS * 3);
        let tail = 100u32..(100 + (SEG_WORDS as u32) / 2);

        let words: Vec<u32> = multi_chain!(
            flash_a.clone(),
            flash_b.clone(),
            flash_a.clone(),
            fill_zero,
            fill_ones,
            flash_b,
            flash_a,
            tail,
        )
        .collect();

        for max_dl in [seg_bytes, seg_bytes * 2, seg_bytes * 3, 1 << 20] {
            for start in [0, seg_bytes / 2, seg_bytes - U32_SIZE, seg_bytes * 3 + 8] {
                assert_raw_stream_matches_generate(&words, max_dl, seg_bytes, start);
            }
        }
    }

    #[fuchsia::test]
    fn test_raw_stream_iterator_crosses_internal_io_buffer() {
        // Exceed RAW_READ_BUF_BYTES (1 MiB) so multiple refills occur across both Fill and Flash.
        let words_per_seg = 128usize; // 512-byte segments (matches GBL block size)
        let seg_bytes = (words_per_seg as u64) * U32_SIZE;
        let fill_words = (RAW_READ_BUF_BYTES / 4) + words_per_seg * 5;
        let flash_words = (RAW_READ_BUF_BYTES / 4) + words_per_seg * 3;
        let mut words = vec![0u32; fill_words];
        words.extend((0..flash_words as u32).map(|i| i.wrapping_add(1)));
        words.extend(std::iter::repeat_n(0x5a5a_5a5au32, fill_words));

        assert_raw_stream_matches_generate(&words, 512 * 1024, seg_bytes, 512 + 128);
        assert_raw_stream_matches_generate(&words, 4 * 1024 * 1024, seg_bytes, 0);
    }

    #[fuchsia::test]
    fn test_raw_stream_iterator_empty_and_tiny() {
        assert_raw_stream_matches_generate(&[], 4096, 4096, 0);
        assert_raw_stream_matches_generate(&[0u32; 8], 4096, 4096, 0);
        assert_raw_stream_matches_generate(&[1, 2, 3, 4], 4096, 4096, 12);
    }

    #[fuchsia::test]
    fn test_raw_stream_iterator_truncated_reader_stops_after_error() {
        let data = vec![0x11u8; 4096];
        // Declare 8192 bytes on a 4096-byte reader.
        let mut iter =
            RawStreamIterator::new(MemFile::new(&data[..]), 8192, 8192, 4096, 0).unwrap();
        assert!(iter.next().unwrap().is_err(), "truncated raw image must return an error");
        assert!(iter.next().is_none(), "iterator must terminate after an error");
    }
}
