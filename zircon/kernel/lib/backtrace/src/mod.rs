// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{CStr, c_void};
use core::fmt;
use core::ptr::without_provenance;
use libc::stdio::{FILE, fprintf, stdout};
use object_constants_rs as object_constants;

use crate::top::boot_constants::kernel_debug_ident;

mod backtrace_ffi;
#[cfg(target_arch = "x86_64")]
mod cpu_context_exchange;
#[cfg(ktest)]
mod tests;

#[cfg(target_arch = "x86_64")]
pub use cpu_context_exchange::{CpuContext, GLOBAL_CPU_CONTEXT_EXCHANGE, GlobalCpuContextExchange};

const FORMAT: &CStr = c"{{{bt:%zu:%p:%s}}}\n";
const RA: &CStr = c"ra";
const PC: &CStr = c"pc";

/// Contains an array of return address values representing a thread's backtrace.
///
/// This type is not thread-safe.
#[repr(C)]
#[derive(Clone)]
pub struct Backtrace {
    addr: [usize; Backtrace::MAX_SIZE],
    size: usize,
    first_frame_type: FrameType,
}

// The layout is shared with the C++ `Backtrace` in <lib/backtrace.h>.
zr::static_assert!(core::mem::size_of::<FrameType>() == 4);
zr::static_assert!(core::mem::align_of::<FrameType>() == 4);
zr::static_assert!(core::mem::size_of::<Backtrace>() == object_constants::kBacktraceSize);
zr::static_assert!(core::mem::align_of::<Backtrace>() == object_constants::kBacktraceAlign);
zr::static_assert!(core::mem::offset_of!(Backtrace, addr) == 0);
zr::static_assert!(core::mem::offset_of!(Backtrace, size) == 256);
zr::static_assert!(core::mem::offset_of!(Backtrace, first_frame_type) == 264);

/// With the possible exception of the first frame, each address in the backtrace is a return
/// address. The first frame may be either a return address or a precise location (think PC).
/// See the "Presentation elements" section of `//docs/reference/kernel/symbolizer_markup.md` for
/// details. Unless specified, the first frame is assumed to be a return address.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameType {
    #[default]
    ReturnAddress = 0,
    PreciseLocation = 1,
}

impl Backtrace {
    /// The maximum number of elements in a backtrace. The value should be large enough to
    /// capture the full call stack for most kernel crashes, but also small enough that it's
    /// reasonable to allocate a `Backtrace` on the stack.
    pub const MAX_SIZE: usize = 32;

    /// Creates an empty `Backtrace`.
    #[inline]
    pub const fn new() -> Self {
        Self { addr: [0; Self::MAX_SIZE], size: 0, first_frame_type: FrameType::ReturnAddress }
    }

    /// Returns the number of elements in this backtrace.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns the underlying elements.
    #[inline]
    pub fn data(&self) -> &[usize] {
        &self.addr[..self.size.min(Self::MAX_SIZE)]
    }

    /// Resets the size to 0.
    #[inline]
    pub fn reset(&mut self) {
        self.size = 0;
    }

    /// Adds one element to the array. See also [`Self::set_first_frame_type`].
    #[inline]
    pub fn push_back(&mut self, addr: usize) {
        if self.size < Self::MAX_SIZE {
            self.addr[self.size] = addr;
            self.size += 1;
        }
    }

    /// Sets the type of the first frame; see [`FrameType`].
    #[inline]
    pub fn set_first_frame_type(&mut self, ty: FrameType) {
        self.first_frame_type = ty;
    }

    /// Pretty-prints this backtrace to `stdout`.
    ///
    /// Safe to call prior to `call_constructors()` (`ConstructorsCalled() == false`), in which
    /// case output is silently dropped.
    #[inline]
    pub fn print(&self) {
        // SAFETY: `stdout()` is non-null and aligned, pointing to `gStdout`, which is statically
        // initialized with a thread-safe write callback (a safe no-op prior to
        // `call_constructors()`, dropping output) that does not mutate `gStdout`, and is not
        // mutated during normal kernel operation (note that `console_rust` ktests temporarily
        // overwrite `gStdout` assuming quiescent output).
        unsafe { self.print_to_file(stdout()) };
    }

    /// Pretty-prints this backtrace to `file`.
    ///
    /// # Safety
    ///
    /// `file` must be non-null, properly aligned, and point to an initialized `FILE` with a
    /// non-null write callback that is valid to call for the duration of this method, without
    /// `*file` being mutated.
    pub unsafe fn print_to_file(&self, file: *mut FILE) {
        // SAFETY: The caller guarantees that `file` is non-null, aligned, and points to an
        // initialized `FILE` with a valid non-null write callback and that `*file` is not mutated
        // for the duration of the call.
        unsafe {
            FILE::write_raw(file, kernel_debug_ident());
            self.print_without_version(file);
        }
    }

    /// Pretty-prints this backtrace to `file`, without the `kernel_debug_ident` preamble that
    /// [`Self::print_to_file`] writes first.
    ///
    /// # Safety
    ///
    /// `file` must be non-null, properly aligned, and point to an initialized `FILE` with a
    /// non-null write callback that is valid to call for the duration of this method, without
    /// `*file` being mutated.
    pub unsafe fn print_without_version(&self, file: *mut FILE) {
        for (i, &addr) in self.data().iter().enumerate() {
            let mut ty = RA;
            if self.first_frame_type == FrameType::PreciseLocation && i == 0 {
                ty = PC;
            }
            // SAFETY: `FORMAT` is NUL-terminated and its conversions match the arguments:
            // `%zu` <- `usize`, `%p` <- `*const c_void` (via `without_provenance(addr)`), and
            // `%s` <- the NUL-terminated `ty`. The caller guarantees that `file` is non-null,
            // aligned, and points to an initialized `FILE` with a valid non-null write callback
            // and that `*file` is not mutated for the duration of the call.
            unsafe {
                fprintf(file, FORMAT.as_ptr(), i, without_provenance::<c_void>(addr), ty.as_ptr())
            };
        }
    }
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backtrace")
            .field("addr", &self.data())
            .field("first_frame_type", &self.first_frame_type)
            .finish()
    }
}

impl Default for Backtrace {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
