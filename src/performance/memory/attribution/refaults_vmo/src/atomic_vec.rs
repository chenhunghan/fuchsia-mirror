// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cmp::min;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

const BITS: u64 = u64::BITS as u64;

fn new_words(nwords: u64) -> Box<[AtomicU64]> {
    (0..nwords).map(|_| AtomicU64::new(0)).collect()
}

mod private {
    /// Prevents other crates from implementing Overflow, as it is tightly coupled with
    /// [`AtomicBitVec`] internals.
    pub trait Sealed {}
}

/// Holds the words of an [`AtomicBitVec`] that don't fit in its initial allocation.
///
/// This is what makes an `AtomicBitVec` growable or not: [`NoOverflow`] is a zero-sized type, so a
/// vector that is never grown doesn't pay for the ability to grow.
pub trait Overflow: private::Sealed + Default {
    /// Returns the word at `index`, counted from the first word past the initial allocation,
    /// allocating it if needed, or `None` if the vector cannot grow.
    fn word(&self, index: u64) -> Option<&AtomicU64>;
}

/// Overflow for a bit vector that cannot grow. See [`Overflow`].
#[derive(Default)]
pub struct NoOverflow;

impl private::Sealed for NoOverflow {}

impl Overflow for NoOverflow {
    fn word(&self, _index: u64) -> Option<&AtomicU64> {
        None
    }
}

/// Overflow for a bit vector that can grow. See [`Overflow`].
///
/// Words are held in a chain of segments, appended when a word past the end of the chain is
/// requested. Segments are neither moved nor freed once published, so references to the words they
/// hold stay valid for as long as the vector. `OnceLock` publishes a segment to the other threads
/// walking the chain: one that doesn't observe it enters `get_or_init`, which waits for the
/// initialization it missed.
#[derive(Default)]
pub struct Growable {
    head: OnceLock<Box<Segment>>,
}

impl private::Sealed for Growable {}

impl Overflow for Growable {
    fn word(&self, mut index: u64) -> Option<&AtomicU64> {
        // A segment that holds `index` holds at least as many words as the whole chain, since
        // `index` counts from the end of the initial allocation. Growing a vector repeatedly
        // therefore allocates a logarithmic number of segments.
        let nwords = index + 1;
        let mut next = &self.head;
        loop {
            // Append a segment if we reached the end of the chain. If another thread wins the
            // race, we adopt its segment, which may be too short, and keep walking.
            let segment = next.get_or_init(|| Box::new(Segment::new(nwords)));
            match segment.words.get(index as usize) {
                Some(word) => return Some(word),
                None => index -= segment.words.len() as u64,
            }
            next = &segment.next;
        }
    }
}

/// A chunk of storage appended to a [`Growable`] bit vector.
struct Segment {
    words: Box<[AtomicU64]>,
    next: OnceLock<Box<Segment>>,
}

impl Segment {
    fn new(nwords: u64) -> Self {
        Self { words: new_words(nwords), next: OnceLock::new() }
    }
}

/// An atomic bit-vector.
///
/// By default the vector has a fixed size, and setting a bit past its end panics.
/// `AtomicBitVec<Growable>` instead grows to hold that bit, which appends storage rather than
/// reallocating it, so that a word handed out to a thread stays valid while another thread grows
/// the vector.
pub struct AtomicBitVec<O: Overflow = NoOverflow> {
    /// Words `0..storage.len()`. Allocated once, and never reallocated.
    storage: Box<[AtomicU64]>,
    /// The words past the end of `storage`. Zero-sized unless the vector is growable.
    overflow: O,
}

impl<O: Overflow> AtomicBitVec<O> {
    /// Creates a new `AtomicBitVec` holding at least `nbits` bits, all set to false.
    pub fn new(nbits: u64) -> Self {
        Self { storage: new_words(nbits.div_ceil(BITS)), overflow: O::default() }
    }

    /// Sets the bits between `start_bit` (included) and `end_bit` (excluded), and returns the
    /// number of bits that were already set. A growable vector grows to hold `end_bit`.
    pub fn test_and_set_range(&self, start_bit: u64, end_bit: u64) -> u64 {
        if start_bit >= end_bit {
            return 0;
        }

        let mut counter = 0;
        let mut current_bit = start_bit;

        while current_bit < end_bit {
            let current_word_index = current_bit / BITS;
            let current_word_start_bit = current_word_index * BITS;
            let mask =
                Self::get_mask(current_bit % BITS, min(end_bit - current_word_start_bit, BITS));
            let old_word = self.word(current_word_index).fetch_or(mask, Ordering::Relaxed);
            counter += (old_word & mask).count_ones();
            current_bit = current_word_start_bit + BITS;
        }

        counter.into()
    }

    /// Copy the first `nbits` bits. Test only.
    pub fn to_vec(&self, nbits: u64) -> Vec<bool> {
        let fetch = |bit: u64| {
            let bit_mask = 1 << (bit % BITS);
            self.word(bit / BITS).load(Ordering::Relaxed) & bit_mask != 0
        };
        (0..nbits).map(fetch).collect()
    }

    /// Returns the word at `index`, which a fixed-size vector must hold, and which a growable one
    /// grows to hold.
    fn word(&self, index: u64) -> &AtomicU64 {
        match self.storage.get(index as usize) {
            Some(word) => word,
            None => self.overflow_word(index - self.storage.len() as u64),
        }
    }

    /// Returns the overflow word at `index`, counted from the end of `storage`. Cold because most
    /// files will not be resized.
    #[cold]
    fn overflow_word(&self, index: u64) -> &AtomicU64 {
        self.overflow.word(index).expect("word index outside of the vector")
    }

    fn get_mask(start_bit: u64, end_bit: u64) -> u64 {
        let left_mask = u64::MAX << start_bit;
        let right_mask = u64::MAX >> (BITS - end_bit);
        left_mask & right_mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let vec = AtomicBitVec::<NoOverflow>::new(0);
        assert_eq!(vec.storage.len(), 0);

        let vec = AtomicBitVec::<NoOverflow>::new(1);
        assert_eq!(vec.storage.len(), 1);
        assert_eq!(vec.storage[0].load(Ordering::Relaxed), 0);

        let vec = AtomicBitVec::<NoOverflow>::new(64);
        assert_eq!(vec.storage.len(), 1);
        assert_eq!(vec.storage[0].load(Ordering::Relaxed), 0);

        let vec = AtomicBitVec::<NoOverflow>::new(65);
        assert_eq!(vec.storage.len(), 2);
        assert_eq!(vec.storage[0].load(Ordering::Relaxed), 0);
        assert_eq!(vec.storage[1].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_size() {
        // A vector that cannot grow pays nothing for the ability to grow: `FxBlob` holds one, and
        // sits exactly on its own size budget.
        assert!(size_of::<AtomicBitVec>() <= 16);
        assert!(size_of::<AtomicBitVec<Growable>>() <= 32);
    }

    #[test]
    fn test_test_and_set() {
        let vec = AtomicBitVec::<NoOverflow>::new(320);

        // Set bits and check they were not set before.
        assert_eq!(vec.test_and_set_range(10, 20), 0);
        // Check they are set now.
        assert_eq!(vec.test_and_set_range(10, 20), 10);

        // Check another range, partially overlapping.
        assert_eq!(vec.test_and_set_range(15, 25), 5);

        // A range across two words.
        assert_eq!(vec.test_and_set_range(60, 70), 0);
        // Only 10 more.
        assert_eq!(vec.test_and_set_range(55, 75), 10);

        // Large range.
        assert_eq!(vec.test_and_set_range(50, 300), 20);
        assert_eq!(vec.test_and_set_range(50, 300), 250);
    }

    #[test]
    fn test_test_and_set2() {
        let vec = AtomicBitVec::<NoOverflow>::new(320);
        assert_eq!(vec.test_and_set_range(64, 128), 0);
        assert_eq!(vec.test_and_set_range(64, 128), 64);
    }

    #[test]
    fn test_test_and_set3() {
        let vec = AtomicBitVec::<NoOverflow>::new(150);

        assert_eq!(vec.test_and_set_range(0, 1), 0);
        assert_eq!(vec.test_and_set_range(63, 64), 0);
        // 0 and 63 are already set.
        assert_eq!(vec.test_and_set_range(0, 64), 2);
        assert_eq!(vec.test_and_set_range(64, 65), 0);
        // 63 and 64 are already set.
        assert_eq!(vec.test_and_set_range(63, 65), 2);

        // 63 and 64 are already set.
        assert_eq!(vec.test_and_set_range(63, 128), 2);
        // 63 to 127 (included) are already set.
        assert_eq!(vec.test_and_set_range(63, 129), 65);
    }

    // A fixed-size vector panics once a write reaches a word it does not hold.
    #[test]
    #[should_panic]
    fn test_and_set_out_of_bounds() {
        let vec = AtomicBitVec::<NoOverflow>::new(10);
        vec.test_and_set_range(64, 65);
    }

    #[test]
    fn test_grow() {
        // A growable vector does not panic when a write reaches the end, but it grows.
        let vec = AtomicBitVec::<Growable>::new(10);
        vec.test_and_set_range(0, 10);
        assert_eq!(vec.test_and_set_range(10, 64), 0);
        assert_eq!(vec.test_and_set_range(0, 64), 64);

        // Growing past it appends a segment.
        assert_eq!(vec.test_and_set_range(64, 65), 0);
        assert_eq!(vec.test_and_set_range(0, 65), 65);
    }

    #[test]
    fn test_grow_across_segments() {
        let vec = AtomicBitVec::<Growable>::new(64);

        // Set a range spanning the original storage and two new segments.
        assert_eq!(vec.test_and_set_range(60, 200), 0);

        // The count works, even across segments.
        assert_eq!(vec.test_and_set_range(60, 200), 140);

        // Bits outside of the range were not touched.
        assert_eq!(vec.test_and_set_range(0, 60), 0);
    }

    #[test]
    fn test_concurrent_grow() {
        const THREADS: u64 = 4;
        const ROUNDS: u64 = 1000;

        let vec = std::sync::Arc::new(AtomicBitVec::<Growable>::new(1));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let vec = vec.clone();
                std::thread::spawn(move || {
                    // Every thread races to grow the vector and to set the bits it exposes.
                    // Count the bits that this thread set, i.e. that it found unset.
                    let mut bits_set = 0;
                    for round in 1..=ROUNDS {
                        bits_set += round - vec.test_and_set_range(0, round);
                    }
                    bits_set
                })
            })
            .collect();

        let bits_set: u64 = threads.into_iter().map(|thread| thread.join().unwrap()).sum();

        // Every bit goes from unset to set exactly once, and exactly one thread sees it happen.
        assert_eq!(bits_set, ROUNDS);
        assert!(vec.to_vec(ROUNDS).into_iter().all(|bit| bit));
    }
}
