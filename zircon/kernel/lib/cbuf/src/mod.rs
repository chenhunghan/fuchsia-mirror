// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::Deadline;
use crate::kernel::event::Event;
use crate::kernel::relaxed_atomic::RelaxedAtomicU32;
use core::ptr::NonNull;
use debug::ltracef;
use ksync::LockToken;
use pin_init::PinInit;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

/// Context returned along with a character read from the circular buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadContext {
    /// The character that was read.
    pub c: u8,
    /// Whether the buffer was full before reading.
    pub transitioned_from_full: bool,
}

/// A circular byte buffer, protected by a spinlock.
///
/// `head`, `tail` and `len_pow2` are modified only with `lock` held. They are relaxed atomics so
/// that `full` can read them without it, while every access with the lock held is ordered by the
/// lock.
#[ksync::guarded]
#[repr(C)]
pub struct Cbuf {
    head: RelaxedAtomicU32,
    tail: RelaxedAtomicU32,
    len_pow2: RelaxedAtomicU32,

    #[guarded_by(lock)]
    buf: Option<NonNull<u8>>,

    #[pin]
    event: Event,

    #[mutex]
    lock: ksync::KMutex<crate::ksync_kernel::TrackedSpinlock>,
}

// SAFETY: `buf` is the only field that is not `Send`. The memory it points to stays valid while the
// `Cbuf` is used (the obligation of `initialize`), and is accessed only with `lock` held.
unsafe impl Send for Cbuf {}

// SAFETY: As for `Send`: `buf` is read only with `lock` held, and the bytes it points to are read
// and written only with `lock` held.
unsafe impl Sync for Cbuf {}

impl Cbuf {
    /// Returns a pin initializer for a `Cbuf` with no underlying data buffer.
    ///
    /// A subsequent call to `initialize` must supply the data buffer. Until then the `Cbuf` is
    /// both full and empty: writes are dropped and reads find nothing.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            head: RelaxedAtomicU32::new(0),
            tail: RelaxedAtomicU32::new(0),
            len_pow2: RelaxedAtomicU32::new(0),
            buf: None.into(),
            event <- Event::init_unsignaled(),
            lock <- ksync::KMutex::init(),
        })
    }

    /// Initializes the `Cbuf` to use the `len` bytes at `buf` for internal storage.
    ///
    /// # Safety
    ///
    /// Must be called at most once on a given `Cbuf`. `len` must be a non-zero power of two, and
    /// `buf` must point to `len` writable bytes that remain valid, and are accessed only through
    /// this `Cbuf`, for as long as the `Cbuf` is used.
    pub unsafe fn initialize(&self, len: usize, buf: *mut u8) {
        debug_assert!(len > 0);
        debug_assert!(len.is_power_of_two());

        let len_pow2 = len.ilog2();
        {
            ksync::lock!(let mut guard = self.lock_lock());
            debug_assert!(guard.fields().buf.is_none());
            self.len_pow2.store(len_pow2);
            *guard.as_mut().fields_mut().buf = NonNull::new(buf);
        }

        ltracef!("len {len}, len_pow2 {len_pow2}\n");
    }

    /// Returns `true` if the buffer has no free space.
    ///
    /// Does not take the lock, so without it the answer may be stale by the time the caller acts
    /// on it. `write_char` checks again with the lock held.
    // TODO(https://fxbug.dev/42125783): We want to revisit the Cbuf API. Lock-free `full` was
    // intended for interrupt context on the assumption that no concurrent modifications can occur,
    // which does not exclude concurrent readers on other CPUs or satisfy the Rust memory model;
    // relaxed atomics avoid data races here at no additional cost.
    pub fn full(&self) -> bool {
        let len_pow2 = self.len_pow2.load();
        let head = self.head.load();
        let tail = self.tail.load();
        let consumed = modpow2(head.wrapping_sub(tail), len_pow2);
        let avail = valpow2(len_pow2) - consumed - 1;
        avail == 0
    }

    /// Writes a single character to the buffer if space is available.
    ///
    /// Returns `1` if the write succeeded, or `0` if the buffer was full.
    pub fn write_char(&self, c: u8) -> usize {
        {
            ksync::lock!(let guard = self.lock_lock());

            if self.full() {
                return 0;
            }
            let Some(buf) = *guard.fields().buf else {
                return 0;
            };

            let head = self.head.load();
            // SAFETY: `head` is below the `len` passed to the single `initialize` call, whose
            // caller guarantees that `buf` points to `len` writable bytes. `lock` is held.
            unsafe { buf.as_ptr().add(head as usize).write(c) };
            self.inc_pointer(&self.head, 1, guard.token());
        }

        // By signaling after dropping the lock, we avoid lock thrashing (though it doesn't matter
        // much since this lock is a spinlock).
        //
        // Note: by the time we signal, the buffer may have already been drained, but that's OK.
        // It just means a reader may be woken when the buffer is empty.
        self.event.signal();

        1
    }

    /// Reads a single character from the buffer, returning it along with the transition context.
    ///
    /// If `block` is true, blocks until a character is read or the thread is asked to suspend or
    /// terminate (`ZX_ERR_INTERNAL_INTR_RETRY`, `ZX_ERR_INTERNAL_INTR_KILLED`).
    ///
    /// If `block` is false and no character is ready, returns `Err(Status::SHOULD_WAIT)`.
    pub fn read_char_with_context(&self, block: bool) -> Result<ReadContext, Status> {
        loop {
            {
                ksync::lock!(let guard = self.lock_lock());
                let token = guard.token();
                if !self.empty(token)
                    && let Some(buf) = *guard.fields().buf
                {
                    let tail = self.tail.load();
                    let res = ReadContext {
                        // SAFETY: `tail` is below the `len` passed to the single `initialize` call,
                        // whose caller guarantees that `buf` points to `len` valid bytes. `lock` is
                        // held.
                        c: unsafe { buf.as_ptr().add(tail as usize).read() },
                        transitioned_from_full: self.full(),
                    };

                    self.inc_pointer(&self.tail, 1, token);
                    if self.empty(token) {
                        let _ = self.event.unsignal();
                    }
                    return Ok(res);
                }

                // Because the signal state does not 100% match the buffer state, it is critical
                // that the event is unsignaled when the buffer is found to be empty (not just when
                // it *transitions* to empty).
                let _ = self.event.unsignal();
            }

            if !block {
                return Err(Status::SHOULD_WAIT);
            }

            self.event.wait(&Deadline::infinite())?;
        }
    }

    /// Reads one character.
    ///
    /// If `block` is true, blocks until a character is read or the thread is asked to suspend or
    /// terminate (`ZX_ERR_INTERNAL_INTR_RETRY`, `ZX_ERR_INTERNAL_INTR_KILLED`).
    ///
    /// If `block` is false and no character is ready, returns `Err(Status::SHOULD_WAIT)`.
    pub fn read_char(&self, block: bool) -> Result<u8, Status> {
        self.read_char_with_context(block).map(|ctx| ctx.c)
    }

    /// Returns true iff the buffer is empty.
    fn empty(&self, _token: &LockToken<'_, CbufLockClass>) -> bool {
        self.tail.load() == self.head.load()
    }

    fn inc_pointer(&self, ptr: &RelaxedAtomicU32, inc: u32, _token: &LockToken<'_, CbufLockClass>) {
        let len_pow2 = self.len_pow2.load();
        ptr.store(modpow2(ptr.load().wrapping_add(inc), len_pow2));
    }
}

#[inline]
fn modpow2(val: u32, modp2: u32) -> u32 {
    val & ((1u32 << modp2) - 1)
}

#[inline]
fn valpow2(valp2: u32) -> u32 {
    1u32 << valp2
}

/// Test suite for Rust cbuf implementation.
#[cfg(ktest)]
#[unittest::suite(name = "cbuf_rust")]
mod tests {
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU32, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_ok, assert_true, unwrap_ok};
    use zx_status::Status;
    use zx_types::ZX_ERR_INTERNAL_INTR_KILLED;

    use crate::kernel::thread;

    /// Test that the cbuf constructor initializes it to full until initialized.
    #[test]
    fn constructor() {
        stack_pin_init!(let cbuf = Cbuf::init());
        assert_true!(cbuf.full());

        let mut buf = [0u8; 4];
        // SAFETY: First and only `initialize` call on `cbuf`; `buf` is 4 (a power of two) bytes,
        // outlives every use of `cbuf`, and is not otherwise accessed.
        unsafe {
            cbuf.initialize(buf.len(), buf.as_mut_ptr());
        }
        assert_true!(!cbuf.full());
    }

    /// Test basic read and write operations.
    #[test]
    fn read_write() {
        stack_pin_init!(let cbuf = Cbuf::init());

        let mut buf = [0u8; 4];
        // SAFETY: First and only `initialize` call on `cbuf`; `buf` is 4 (a power of two) bytes,
        // outlives every use of `cbuf`, and is not otherwise accessed.
        unsafe {
            cbuf.initialize(buf.len(), buf.as_mut_ptr());
        }

        assert_true!(!cbuf.full());

        // Nothing to read, don't wait.
        assert_true!(cbuf.read_char(false) == Err(Status::SHOULD_WAIT));

        // Write some characters.
        let data = b"ABC";
        for &c in data {
            assert_eq!(cbuf.write_char(c), 1);
        }
        assert_true!(cbuf.full());

        // Writing when full should return 0.
        assert_eq!(cbuf.write_char(b'D'), 0);

        // Read them back.
        for (i, &expected) in data.iter().enumerate() {
            let res = unwrap_ok!(cbuf.read_char_with_context(true));
            assert_eq!(res.transitioned_from_full, i == 0);
            assert_eq!(res.c, expected);
        }
        assert_true!(!cbuf.full());
    }

    extern "C" fn reader_thread_entry(arg: *mut c_void) -> i32 {
        // SAFETY: arg is a valid pointer to a Cbuf pinned on the parent thread's stack.
        let cbuf = unsafe { &*(arg as *const Cbuf) };
        loop {
            match cbuf.read_char(true) {
                Ok(_) => {}
                Err(status) => return status.into_raw(),
            }
        }
    }

    /// Test concurrent read and write operations to check for races.
    #[test]
    fn read_write_race() {
        stack_pin_init!(let cbuf = Cbuf::init());

        let mut buf = [0u8; 4];
        // SAFETY: First and only `initialize` call on `cbuf`; `buf` is 4 (a power of two) bytes,
        // outlives every use of `cbuf`, and is not otherwise accessed.
        unsafe {
            cbuf.initialize(buf.len(), buf.as_mut_ptr());
        }

        let thread_name = c"cbuf_rust_race".as_ptr();
        let cbuf_ptr = &*cbuf as *const Cbuf as *mut c_void;

        // SAFETY: we pass reader_thread_entry and valid pointers. The thread is joined
        // before `cbuf` (and `buf`) goes out of scope.
        unsafe {
            let thread = unwrap_ok!(thread::spawn(thread_name, reader_thread_entry, cbuf_ptr));

            for _ in 0..1000 {
                while cbuf.write_char(b'A') == 0 {
                    thread::r#yield();
                }
            }

            thread.kill();

            let ret = unwrap_ok!(thread.join(InstantMono::INFINITE));
            assert_eq!(ret, ZX_ERR_INTERNAL_INTR_KILLED);
        }
    }

    /// Test uninitialized cbuf operations.
    #[test]
    fn uninitialized() {
        stack_pin_init!(let cbuf = Cbuf::init());

        assert_true!(cbuf.full());
        assert_eq!(cbuf.write_char(b'A'), 0);
        assert_true!(cbuf.read_char(false) == Err(Status::SHOULD_WAIT));
    }

    /// Test buffer wrap around behavior.
    #[test]
    fn wrap_around() {
        stack_pin_init!(let cbuf = Cbuf::init());
        let mut buf = [0u8; 4];

        // SAFETY: First and only `initialize` call on `cbuf`; `buf` is 4 (a power of two) bytes,
        // outlives every use of `cbuf`, and is not otherwise accessed.
        unsafe {
            cbuf.initialize(buf.len(), buf.as_mut_ptr());
        }

        // Write 3 chars (capacity is 3)
        assert_eq!(cbuf.write_char(b'A'), 1);
        assert_eq!(cbuf.write_char(b'B'), 1);
        assert_eq!(cbuf.write_char(b'C'), 1);

        assert_true!(cbuf.full());

        // Read 3 chars
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'A');
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'B');
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'C');

        assert_true!(!cbuf.full());

        // Write 2 chars (wraps pointers)
        assert_eq!(cbuf.write_char(b'D'), 1);
        assert_eq!(cbuf.write_char(b'E'), 1);

        // Read 2 chars (wraps pointers)
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'D');
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'E');

        // Should be empty
        assert_true!(cbuf.read_char(false) == Err(Status::SHOULD_WAIT));
    }

    struct BlockingReadContext {
        cbuf: *mut Cbuf,
        state: *const AtomicU32, // 0: init, 1: about to read, 2: read done, 3: error
        read_char: *mut u8,
    }

    // SAFETY: We only pass valid pointers and don't share mutability unsafely.
    unsafe impl Send for BlockingReadContext {}

    extern "C" fn blocking_reader_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` is the `BlockingReadContext` on `blocking_read`'s stack, and its pointers
        // are to locals of `blocking_read`, which joins this thread before they go out of scope.
        let ctx = unsafe { &*(arg as *const BlockingReadContext) };
        // SAFETY: As above.
        let cbuf = unsafe { &*ctx.cbuf };
        // SAFETY: As above.
        let state = unsafe { &*ctx.state };

        state.store(1, Ordering::SeqCst);
        let c = cbuf.read_char(true); // Should block until written.

        match c {
            Ok(val) => {
                // SAFETY: As above. `blocking_read` reads `read_char` only after the join.
                unsafe { *ctx.read_char = val };
                state.store(2, Ordering::SeqCst);
                0
            }
            Err(status) => {
                state.store(3, Ordering::SeqCst); // error
                status.into_raw()
            }
        }
    }

    /// Test blocking read operation.
    #[test]
    fn blocking_read() {
        stack_pin_init!(let cbuf = Cbuf::init());

        let mut buf = [0u8; 4];
        // SAFETY: First and only `initialize` call on `cbuf`; `buf` is 4 (a power of two) bytes,
        // outlives every use of `cbuf`, and is not otherwise accessed.
        unsafe {
            cbuf.initialize(buf.len(), buf.as_mut_ptr());
        }

        let state = AtomicU32::new(0);
        let mut read_char = 0u8;

        let mut ctx = BlockingReadContext {
            cbuf: &*cbuf as *const Cbuf as *mut Cbuf,
            state: &state,
            read_char: &mut read_char,
        };

        let thread_name = c"cbuf_blocking_read".as_ptr();
        let ctx_ptr = &mut ctx as *mut BlockingReadContext as *mut c_void;

        // SAFETY: `ctx` and the locals it points to outlive the thread, which is joined below.
        unsafe {
            let thread = unwrap_ok!(thread::spawn(thread_name, blocking_reader_entry, ctx_ptr));

            // Wait until the reader thread is about to read.
            while state.load(Ordering::SeqCst) < 1 {
                thread::r#yield();
            }

            // Wait until the reader thread is actually blocked.
            while !thread.is_blocked() {
                thread::r#yield();
                // If it failed and exited, break.
                if state.load(Ordering::SeqCst) == 3 {
                    break;
                }
            }

            if state.load(Ordering::SeqCst) == 3 {
                thread.join(InstantMono::INFINITE).ok();
                panic!("reader thread failed early");
            }

            // Double check it is indeed blocked and state is 1.
            assert_true!(thread.is_blocked());
            assert_eq!(state.load(Ordering::SeqCst), 1);

            // Now write a char. This should wake it up.
            assert_eq!(cbuf.write_char(b'X'), 1);

            // Wait for reader thread to complete.
            let ret = unwrap_ok!(thread.join(InstantMono::INFINITE));

            assert_ok!(Status::ok(ret));
            assert_eq!(state.load(Ordering::SeqCst), 2);
            assert_eq!(read_char, b'X');
        }
    }
}
