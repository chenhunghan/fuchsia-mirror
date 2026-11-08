// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::memio::EbpfPtr;
use crate::{
    BPF_STACK_SIZE, BpfValue, DataWidth, EbpfInstruction, EbpfProgramContext, FromBpfValue,
    HelperSet, Packet,
};
use ebpf_core::executor::{AtomicRmwOp, EbpfMachineBackend, execute_core};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub fn execute<C: EbpfProgramContext>(
    code: &[EbpfInstruction],
    helpers: &HelperSet<C>,
    run_context: &mut C::RunContext<'_>,
    arguments: &[BpfValue],
) -> u64 {
    assert!(arguments.len() <= 5);
    let mut stack =
        [MaybeUninit::<BpfValue>::uninit(); BPF_STACK_SIZE / std::mem::size_of::<BpfValue>()];
    let stack_ptr = stack.as_mut_ptr();
    let stack_top = (stack_ptr as u64).wrapping_add(BPF_STACK_SIZE as u64);

    let mut raw_args = [0u64; 5];
    for (i, v) in arguments.iter().enumerate() {
        raw_args[i] = v.as_u64();
    }

    let mut backend = NativeMachineBackend { helpers, run_context, stack_top };
    let result = execute_core(code, &mut backend, &raw_args)
        .expect("Invalid code passed to eBPF interpreter");
    // `stack_top` is passed into `backend` as a raw `u64`, so keep `stack_ptr`
    // live and prevent dead-store / lifetime shrinking before `execute_core`
    // returns.
    std::hint::black_box(stack_ptr);
    result
}

struct NativeMachineBackend<'a, 'b, 'c, C: EbpfProgramContext> {
    helpers: &'a HelperSet<C>,
    run_context: &'b mut C::RunContext<'c>,
    stack_top: u64,
}

impl<C: EbpfProgramContext> EbpfMachineBackend for NativeMachineBackend<'_, '_, '_, C> {
    #[inline(always)]
    fn stack_top(&self) -> u64 {
        self.stack_top
    }

    #[inline(always)]
    fn load_memory(&self, addr: u64, width: DataWidth) -> u64 {
        let addr = BpfValue::from(addr);
        match width {
            DataWidth::U8 => {
                // SAFETY: Verifier ensures that the load is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u8>()).load_relaxed() as u64 }
            }
            DataWidth::U16 => {
                // SAFETY: Verifier ensures that the load is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u16>()).load_relaxed() as u64 }
            }
            DataWidth::U32 => {
                // SAFETY: Verifier ensures that the load is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u32>()).load_relaxed() as u64 }
            }
            DataWidth::U64 => {
                // SAFETY: Verifier ensures that the load is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u64>()).load_relaxed() }
            }
        }
    }

    #[inline(always)]
    fn store_memory(&mut self, addr: u64, value: u64, width: DataWidth) {
        let addr = BpfValue::from(addr);
        match width {
            DataWidth::U8 => {
                // SAFETY: Verifier ensures that the store operation is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u8>()).store_relaxed(value as u8) }
            }
            DataWidth::U16 => {
                // SAFETY: Verifier ensures that the store operation is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u16>()).store_relaxed(value as u16) }
            }
            DataWidth::U32 => {
                // SAFETY: Verifier ensures that the store operation is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u32>()).store_relaxed(value as u32) }
            }
            DataWidth::U64 => {
                // SAFETY: Verifier ensures that the store operation is safe.
                unsafe { EbpfPtr::new(addr.as_ptr::<u64>()).store_relaxed(value) }
            }
        }
    }

    #[inline(always)]
    fn atomic_rmw32(&mut self, addr: u64, value: u32, op: AtomicRmwOp) -> u32 {
        let ptr = BpfValue::from(addr).as_ptr::<AtomicU32>();
        // SAFETY: The address has been verified by the verifier to be valid for
        // reading and writing, and suitably aligned for a 32-bit atomic access.
        let atomic = unsafe { &*ptr };
        match op {
            AtomicRmwOp::Add => atomic.fetch_add(value, Ordering::SeqCst),
            AtomicRmwOp::And => atomic.fetch_and(value, Ordering::SeqCst),
            AtomicRmwOp::Or => atomic.fetch_or(value, Ordering::SeqCst),
            AtomicRmwOp::Xor => atomic.fetch_xor(value, Ordering::SeqCst),
            AtomicRmwOp::Xchg => atomic.swap(value, Ordering::SeqCst),
        }
    }

    #[inline(always)]
    fn atomic_rmw64(&mut self, addr: u64, value: u64, op: AtomicRmwOp) -> u64 {
        let ptr = BpfValue::from(addr).as_ptr::<AtomicU64>();
        // SAFETY: The address has been verified by the verifier to be valid for
        // reading and writing, and suitably aligned for a 64-bit atomic access.
        let atomic = unsafe { &*ptr };
        match op {
            AtomicRmwOp::Add => atomic.fetch_add(value, Ordering::SeqCst),
            AtomicRmwOp::And => atomic.fetch_and(value, Ordering::SeqCst),
            AtomicRmwOp::Or => atomic.fetch_or(value, Ordering::SeqCst),
            AtomicRmwOp::Xor => atomic.fetch_xor(value, Ordering::SeqCst),
            AtomicRmwOp::Xchg => atomic.swap(value, Ordering::SeqCst),
        }
    }

    #[inline(always)]
    fn atomic_cmpxchg32(&mut self, addr: u64, expected: u32, new: u32) -> u32 {
        let ptr = BpfValue::from(addr).as_ptr::<AtomicU32>();
        // SAFETY: The address has been verified by the verifier to be valid for
        // reading and writing, and suitably aligned for a 32-bit atomic access.
        let atomic = unsafe { &*ptr };
        match atomic.compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(v) | Err(v) => v,
        }
    }

    #[inline(always)]
    fn atomic_cmpxchg64(&mut self, addr: u64, expected: u64, new: u64) -> u64 {
        let ptr = BpfValue::from(addr).as_ptr::<AtomicU64>();
        // SAFETY: The address has been verified by the verifier to be valid for
        // reading and writing, and suitably aligned for a 64-bit atomic access.
        let atomic = unsafe { &*ptr };
        match atomic.compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(v) | Err(v) => v,
        }
    }

    #[inline(always)]
    fn call_helper(&mut self, index: u32, r1: u64, r2: u64, r3: u64, r4: u64, r5: u64) -> u64 {
        let helper = self.helpers.get_by_index(index).unwrap();
        helper.0(
            self.run_context,
            BpfValue::from(r1),
            BpfValue::from(r2),
            BpfValue::from(r3),
            BpfValue::from(r4),
            BpfValue::from(r5),
        )
        .as_u64()
    }

    #[inline(always)]
    fn load_from_packet(&mut self, packet_ptr: u64, offset: i32, width: DataWidth) -> Option<u64> {
        // SAFETY: The verifier checks that `packet_ptr` (from R6) points at the packet.
        let packet =
            unsafe { C::Packet::from_bpf_value(self.run_context, BpfValue::from(packet_ptr)) };
        packet.load(offset, width).map(|v| v.as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        BPF_ADD, BPF_ALU, BPF_ALU64, BPF_ATOMIC, BPF_CALL, BPF_CMPXCHG, BPF_DW, BPF_EXIT,
        BPF_FETCH, BPF_IND, BPF_JLT, BPF_JMP, BPF_K, BPF_LD, BPF_LDDW, BPF_LDX, BPF_MEM, BPF_MOV,
        BPF_PSEUDO_MAP_IDX, BPF_STX, BPF_W, BPF_X,
    };
    use crate::error::VerifierErrorKind;

    #[derive(Default)]
    struct MockBackend {
        mem64: u64,
        mem32: u32,
        last_helper: Option<(u32, [u64; 5])>,
    }

    impl EbpfMachineBackend for MockBackend {
        fn stack_top(&self) -> u64 {
            0x2000
        }

        fn load_memory(&self, _addr: u64, width: DataWidth) -> u64 {
            match width {
                DataWidth::U32 => self.mem32 as u64,
                DataWidth::U64 => self.mem64,
                _ => 0,
            }
        }

        fn store_memory(&mut self, _addr: u64, value: u64, width: DataWidth) {
            match width {
                DataWidth::U32 => self.mem32 = value as u32,
                DataWidth::U64 => self.mem64 = value,
                _ => {}
            }
        }

        fn atomic_rmw32(&mut self, _addr: u64, value: u32, op: AtomicRmwOp) -> u32 {
            let old = self.mem32;
            self.mem32 = match op {
                AtomicRmwOp::Add => old.wrapping_add(value),
                AtomicRmwOp::And => old & value,
                AtomicRmwOp::Or => old | value,
                AtomicRmwOp::Xor => old ^ value,
                AtomicRmwOp::Xchg => value,
            };
            old
        }

        fn atomic_rmw64(&mut self, _addr: u64, value: u64, op: AtomicRmwOp) -> u64 {
            let old = self.mem64;
            self.mem64 = match op {
                AtomicRmwOp::Add => old.wrapping_add(value),
                AtomicRmwOp::And => old & value,
                AtomicRmwOp::Or => old | value,
                AtomicRmwOp::Xor => old ^ value,
                AtomicRmwOp::Xchg => value,
            };
            old
        }

        fn atomic_cmpxchg32(&mut self, _addr: u64, expected: u32, new: u32) -> u32 {
            let old = self.mem32;
            if old == expected {
                self.mem32 = new;
            }
            old
        }

        fn atomic_cmpxchg64(&mut self, _addr: u64, expected: u64, new: u64) -> u64 {
            let old = self.mem64;
            if old == expected {
                self.mem64 = new;
            }
            old
        }

        fn call_helper(&mut self, index: u32, r1: u64, r2: u64, r3: u64, r4: u64, r5: u64) -> u64 {
            self.last_helper = Some((index, [r1, r2, r3, r4, r5]));
            r1.wrapping_add(r2)
        }

        fn load_from_packet(
            &mut self,
            packet_ptr: u64,
            offset: i32,
            _width: DataWidth,
        ) -> Option<u64> {
            if packet_ptr == 0xdead_beef && offset >= 0 && offset <= 16 {
                Some(0x42 + offset as u64)
            } else {
                None
            }
        }
    }

    #[test]
    fn test_execute_core_alu32_zero_ext_and_lddw() {
        let mut backend = MockBackend::default();
        let code = [
            // r0 = 0x11223344_55667788
            EbpfInstruction::new(BPF_LDDW, 0, 0, 0, 0x55667788u32 as i32),
            EbpfInstruction::new(0, 0, 0, 0, 0x11223344u32 as i32),
            // 32-bit add zero-extends upper 32 bits of r0.
            EbpfInstruction::new(BPF_ALU | BPF_ADD | BPF_K, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(execute_core(&code, &mut backend, &[0; 5]), Ok(0x55667789));
    }

    #[test]
    fn test_execute_core_atomics_and_helper_call() {
        let mut backend = MockBackend { mem64: 10, mem32: 7, last_helper: None };
        let code = [
            // r2 = 5; lock fetch add [r10 - 8], r2 -> old 10 in r2, mem64 = 15
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, 5),
            EbpfInstruction::new(
                BPF_STX | BPF_ATOMIC | BPF_DW,
                10,
                2,
                -8,
                (BPF_ADD | BPF_FETCH) as i32,
            ),
            // r0 = 15 (expected), r3 = 99 (new); lock cmpxchg [r10 - 8], r3 -> r0 = 15, mem64 = 99
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 15),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 3, 0, 0, 99),
            EbpfInstruction::new(BPF_STX | BPF_ATOMIC | BPF_DW, 10, 3, -8, BPF_CMPXCHG as i32),
            // call helper 7 with r1 (from args = 20) and r2 (= 10) -> r0 = 30
            EbpfInstruction::new(BPF_JMP | BPF_CALL, 0, 0, 0, 7),
            // r1 = [r10 - 8] (= 99); r0 += r1 (= 129)
            EbpfInstruction::new(BPF_LDX | BPF_MEM | BPF_DW, 1, 10, -8, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_X, 0, 1, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(execute_core(&code, &mut backend, &[20, 0, 0, 0, 0]), Ok(129));
        assert_eq!(backend.mem64, 99);
        assert_eq!(backend.last_helper, Some((7, [20, 10, 99, 0, 0])));
    }

    #[test]
    fn test_execute_core_packet_load_and_bounds_errors() {
        let mut backend = MockBackend::default();

        // Packet load signed offset overflow aborts cleanly with 0.
        let overflow_pkt = [
            // r6 = r1 (packet ptr), r2 = i32::MAX
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_X, 6, 1, 0, 0),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 2, 0, 0, i32::MAX),
            EbpfInstruction::new(BPF_LD | BPF_IND | BPF_W, 0, 2, 0, 1),
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 99),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(execute_core(&overflow_pkt, &mut backend, &[0xdead_beef, 0, 0, 0, 0]), Ok(0));

        // Falling off the end of `code` returns `FellOffProgramEnd`.
        let no_exit = [EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 1)];
        assert_eq!(
            execute_core(&no_exit, &mut backend, &[0; 5]),
            Err(VerifierErrorKind::FellOffProgramEnd)
        );
        assert_eq!(
            execute_core(&[], &mut backend, &[0; 5]),
            Err(VerifierErrorKind::FellOffProgramEnd)
        );

        // Truncated `BPF_LDDW` returns `IncompleteLddw` instead of panicking.
        let truncated_lddw = [EbpfInstruction::new(BPF_LDDW, 0, 0, 0, 1)];
        assert_eq!(
            execute_core(&truncated_lddw, &mut backend, &[0; 5]),
            Err(VerifierErrorKind::IncompleteLddw)
        );

        // Non-zero second slot fields in `BPF_LDDW` return `MalformedLddw`.
        let malformed_lddw = [
            EbpfInstruction::new(BPF_LDDW, 0, 0, 0, 1),
            EbpfInstruction::new(0, 1, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            execute_core(&malformed_lddw, &mut backend, &[0; 5]),
            Err(VerifierErrorKind::MalformedLddw)
        );

        // Unlinked pseudo-map `src_reg != 0` in `BPF_LDDW` returns `UnsupportedLddwSource`.
        let unlinked_lddw = [
            EbpfInstruction::new(BPF_LDDW, 0, BPF_PSEUDO_MAP_IDX, 0, 0),
            EbpfInstruction::new(0, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(
            execute_core(&unlinked_lddw, &mut backend, &[0; 5]),
            Err(VerifierErrorKind::UnsupportedLddwSource(BPF_PSEUDO_MAP_IDX))
        );

        // Backward jump to `pc = 0` (`1 + 1 + (-2) = 0`) loops without underflowing `pc`.
        let loop_to_zero = [
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_JLT | BPF_K, 0, 0, -2, 3),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        assert_eq!(execute_core(&loop_to_zero, &mut backend, &[0; 5]), Ok(3));
    }
}
