// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::api::{BPF_LDDW, DataWidth, EbpfInstruction, apply_endianness};
use crate::error::VerifierErrorKind;
use crate::visitor::{BpfVisitor, ProgramCounter, Register, Source};

/// Sized to cover all 16 4-bit register nibbles (`0..=15`) in `EbpfInstruction`
/// so masked indexing in `reg`/`set_reg` is total and panic-free.
const REGISTER_SLOTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicRmwOp {
    Add,
    And,
    Or,
    Xor,
    Xchg,
}

/// Abstract host/memory machine interface used by the pure eBPF interpreter loop.
pub trait EbpfMachineBackend {
    fn stack_top(&self) -> u64;
    fn load_memory(&self, addr: u64, width: DataWidth) -> u64;
    fn store_memory(&mut self, addr: u64, value: u64, width: DataWidth);
    fn atomic_rmw32(&mut self, addr: u64, value: u32, op: AtomicRmwOp) -> u32;
    fn atomic_rmw64(&mut self, addr: u64, value: u64, op: AtomicRmwOp) -> u64;
    fn atomic_cmpxchg32(&mut self, addr: u64, expected: u32, new: u32) -> u32;
    fn atomic_cmpxchg64(&mut self, addr: u64, expected: u64, new: u64) -> u64;
    fn call_helper(&mut self, index: u32, r1: u64, r2: u64, r3: u64, r4: u64, r5: u64) -> u64;
    fn load_from_packet(&mut self, packet_ptr: u64, offset: i32, width: DataWidth) -> Option<u64>;
}

/// Executes a verified and linked eBPF program against `backend`.
pub fn execute_core<B: EbpfMachineBackend>(
    code: &[EbpfInstruction],
    backend: &mut B,
    arguments: &[u64; 5],
) -> Result<u64, VerifierErrorKind> {
    let mut state =
        InterpreterState { registers: [0u64; REGISTER_SLOTS], pc: 0, next_imm: 0, result: None };
    let mut i = 0;
    while i < 5 {
        state.set_reg((i as u8) + 1, arguments[i]);
        i += 1;
    }
    state.registers[10] = backend.stack_top();

    while state.result.is_none() {
        if state.pc >= code.len() {
            return Err(VerifierErrorKind::FellOffProgramEnd);
        }
        let inst = code[state.pc];
        state.pc += 1;
        if inst.code() == BPF_LDDW {
            if state.pc >= code.len() {
                return Err(VerifierErrorKind::IncompleteLddw);
            }
            let next_inst = code[state.pc];
            if next_inst.code() != 0
                || next_inst.dst_reg() != 0
                || next_inst.src_reg() != 0
                || next_inst.offset() != 0
            {
                return Err(VerifierErrorKind::MalformedLddw);
            }
            state.next_imm = next_inst.imm();
        } else {
            state.next_imm = 0;
        }
        match state.visit(backend, inst) {
            Ok(()) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(match state.result {
        Some(result) => result,
        None => 0,
    })
}

struct InterpreterState {
    registers: [u64; REGISTER_SLOTS],
    pc: ProgramCounter,
    next_imm: i32,
    result: Option<u64>,
}

impl InterpreterState {
    #[inline(always)]
    fn reg(&self, index: Register) -> u64 {
        self.registers[(index & 0x0f) as usize]
    }

    #[inline(always)]
    fn set_reg(&mut self, index: Register, value: u64) {
        self.registers[(index & 0x0f) as usize] = value;
    }

    #[inline(always)]
    fn advance_pc(&mut self, offset: i16) {
        let next_pc = (self.pc as i64).wrapping_add(offset as i64);
        self.pc = next_pc as usize;
    }

    #[inline(always)]
    fn compute_source(&self, src: Source) -> u64 {
        match src {
            Source::Reg(reg) => self.reg(reg),
            Source::Value(v) => v,
        }
    }

    #[inline(always)]
    fn atomic_rmw32_op<B: EbpfMachineBackend>(
        &mut self,
        backend: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
        op: AtomicRmwOp,
    ) -> Result<(), VerifierErrorKind> {
        let addr = self.reg(dst).wrapping_add(offset as i64 as u64);
        let value = self.reg(src) as u32;
        let old_value = backend.atomic_rmw32(addr, value, op);
        if fetch {
            self.set_reg(src, old_value as u64);
        }
        Ok(())
    }

    #[inline(always)]
    fn atomic_rmw64_op<B: EbpfMachineBackend>(
        &mut self,
        backend: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
        op: AtomicRmwOp,
    ) -> Result<(), VerifierErrorKind> {
        let addr = self.reg(dst).wrapping_add(offset as i64 as u64);
        let value = self.reg(src);
        let old_value = backend.atomic_rmw64(addr, value, op);
        if fetch {
            self.set_reg(src, old_value);
        }
        Ok(())
    }
}

impl<B: EbpfMachineBackend> BpfVisitor<B> for InterpreterState {
    #[inline(always)]
    fn add(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_add(y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn add64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        self.set_reg(dst, x.wrapping_add(y));
        Ok(())
    }

    #[inline(always)]
    fn and(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, (x & y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn and64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        self.set_reg(dst, x & y);
        Ok(())
    }

    #[inline(always)]
    fn arsh(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = (self.reg(dst) as u32) as i32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, (x.wrapping_shr(y) as u32) as u64);
        Ok(())
    }

    #[inline(always)]
    fn arsh64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as i64;
        // `i64::wrapping_shr` masks `y` by 63, matching the eBPF 64-bit shift spec.
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_shr(y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn div(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        let res = if y == 0 { 0 } else { x / y };
        self.set_reg(dst, res as u64);
        Ok(())
    }

    #[inline(always)]
    fn div64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        let res = if y == 0 { 0 } else { x / y };
        self.set_reg(dst, res);
        Ok(())
    }

    #[inline(always)]
    fn lsh(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_shl(y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn lsh64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        // `u64::wrapping_shl` masks `y` by 63, matching the eBPF 64-bit shift spec.
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_shl(y));
        Ok(())
    }

    #[inline(always)]
    fn r#mod(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        let res = if y == 0 { x } else { x % y };
        self.set_reg(dst, res as u64);
        Ok(())
    }

    #[inline(always)]
    fn mod64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        let res = if y == 0 { x } else { x % y };
        self.set_reg(dst, res);
        Ok(())
    }

    #[inline(always)]
    fn mov(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, y as u64);
        Ok(())
    }

    #[inline(always)]
    fn mov64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let y = self.compute_source(src);
        self.set_reg(dst, y);
        Ok(())
    }

    #[inline(always)]
    fn mul(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_mul(y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn mul64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        self.set_reg(dst, x.wrapping_mul(y));
        Ok(())
    }

    #[inline(always)]
    fn or(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, (x | y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn or64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        self.set_reg(dst, x | y);
        Ok(())
    }

    #[inline(always)]
    fn rsh(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_shr(y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn rsh64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        // `u64::wrapping_shr` masks `y` by 63, matching the eBPF 64-bit shift spec.
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_shr(y));
        Ok(())
    }

    #[inline(always)]
    fn sub(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, x.wrapping_sub(y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn sub64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        self.set_reg(dst, x.wrapping_sub(y));
        Ok(())
    }

    #[inline(always)]
    fn xor(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        let y = self.compute_source(src) as u32;
        self.set_reg(dst, (x ^ y) as u64);
        Ok(())
    }

    #[inline(always)]
    fn xor64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
    ) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        let y = self.compute_source(src);
        self.set_reg(dst, x ^ y);
        Ok(())
    }

    #[inline(always)]
    fn neg(&mut self, _context: &mut B, dst: Register) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst) as u32;
        self.set_reg(dst, 0u32.wrapping_sub(x) as u64);
        Ok(())
    }

    #[inline(always)]
    fn neg64(&mut self, _context: &mut B, dst: Register) -> Result<(), VerifierErrorKind> {
        let x = self.reg(dst);
        self.set_reg(dst, 0u64.wrapping_sub(x));
        Ok(())
    }

    #[inline(always)]
    fn be(
        &mut self,
        _context: &mut B,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let new_value = apply_endianness(true, width, self.reg(dst));
        self.set_reg(dst, new_value);
        Ok(())
    }

    #[inline(always)]
    fn le(
        &mut self,
        _context: &mut B,
        dst: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let new_value = apply_endianness(false, width, self.reg(dst));
        self.set_reg(dst, new_value);
        Ok(())
    }

    #[inline(always)]
    fn call_external(&mut self, context: &mut B, index: u32) -> Result<(), VerifierErrorKind> {
        let result = context.call_helper(
            index,
            self.reg(1),
            self.reg(2),
            self.reg(3),
            self.reg(4),
            self.reg(5),
        );
        self.set_reg(0, result);
        Ok(())
    }

    #[inline(always)]
    fn exit(&mut self, _context: &mut B) -> Result<(), VerifierErrorKind> {
        self.result = Some(self.reg(0));
        Ok(())
    }

    #[inline(always)]
    fn jump(&mut self, _context: &mut B, offset: i16) -> Result<(), VerifierErrorKind> {
        self.advance_pc(offset);
        Ok(())
    }

    #[inline(always)]
    fn jeq(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as u32) == (self.compute_source(src) as u32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jeq64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if self.reg(dst) == self.compute_source(src) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jne(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as u32) != (self.compute_source(src) as u32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jne64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if self.reg(dst) != self.compute_source(src) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jge(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as u32) >= (self.compute_source(src) as u32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jge64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if self.reg(dst) >= self.compute_source(src) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jgt(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as u32) > (self.compute_source(src) as u32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jgt64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if self.reg(dst) > self.compute_source(src) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jle(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as u32) <= (self.compute_source(src) as u32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jle64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if self.reg(dst) <= self.compute_source(src) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jlt(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as u32) < (self.compute_source(src) as u32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jlt64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if self.reg(dst) < self.compute_source(src) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jsge(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if ((self.reg(dst) as u32) as i32) >= ((self.compute_source(src) as u32) as i32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jsge64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as i64) >= (self.compute_source(src) as i64) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jsgt(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if ((self.reg(dst) as u32) as i32) > ((self.compute_source(src) as u32) as i32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jsgt64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as i64) > (self.compute_source(src) as i64) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jsle(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if ((self.reg(dst) as u32) as i32) <= ((self.compute_source(src) as u32) as i32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jsle64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as i64) <= (self.compute_source(src) as i64) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jslt(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if ((self.reg(dst) as u32) as i32) < ((self.compute_source(src) as u32) as i32) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jslt64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) as i64) < (self.compute_source(src) as i64) {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jset(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if ((self.reg(dst) as u32) & (self.compute_source(src) as u32)) != 0 {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn jset64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: Source,
        offset: i16,
    ) -> Result<(), VerifierErrorKind> {
        if (self.reg(dst) & self.compute_source(src)) != 0 {
            self.advance_pc(offset);
        }
        Ok(())
    }

    #[inline(always)]
    fn atomic_add(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw32_op(context, fetch, dst, offset, src, AtomicRmwOp::Add)
    }

    #[inline(always)]
    fn atomic_add64(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw64_op(context, fetch, dst, offset, src, AtomicRmwOp::Add)
    }

    #[inline(always)]
    fn atomic_and(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw32_op(context, fetch, dst, offset, src, AtomicRmwOp::And)
    }

    #[inline(always)]
    fn atomic_and64(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw64_op(context, fetch, dst, offset, src, AtomicRmwOp::And)
    }

    #[inline(always)]
    fn atomic_or(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw32_op(context, fetch, dst, offset, src, AtomicRmwOp::Or)
    }

    #[inline(always)]
    fn atomic_or64(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw64_op(context, fetch, dst, offset, src, AtomicRmwOp::Or)
    }

    #[inline(always)]
    fn atomic_xor(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw32_op(context, fetch, dst, offset, src, AtomicRmwOp::Xor)
    }

    #[inline(always)]
    fn atomic_xor64(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw64_op(context, fetch, dst, offset, src, AtomicRmwOp::Xor)
    }

    #[inline(always)]
    fn atomic_xchg(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw32_op(context, fetch, dst, offset, src, AtomicRmwOp::Xchg)
    }

    #[inline(always)]
    fn atomic_xchg64(
        &mut self,
        context: &mut B,
        fetch: bool,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        self.atomic_rmw64_op(context, fetch, dst, offset, src, AtomicRmwOp::Xchg)
    }

    #[inline(always)]
    fn atomic_cmpxchg(
        &mut self,
        context: &mut B,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        let addr = self.reg(dst).wrapping_add(offset as i64 as u64);
        let expected = self.reg(0) as u32;
        let new = self.reg(src) as u32;
        let old = context.atomic_cmpxchg32(addr, expected, new);
        self.set_reg(0, old as u64);
        Ok(())
    }

    #[inline(always)]
    fn atomic_cmpxchg64(
        &mut self,
        context: &mut B,
        dst: Register,
        offset: i16,
        src: Register,
    ) -> Result<(), VerifierErrorKind> {
        let addr = self.reg(dst).wrapping_add(offset as i64 as u64);
        let expected = self.reg(0);
        let new = self.reg(src);
        let old = context.atomic_cmpxchg64(addr, expected, new);
        self.set_reg(0, old);
        Ok(())
    }

    #[inline(always)]
    fn load(
        &mut self,
        context: &mut B,
        dst: Register,
        offset: i16,
        src: Register,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let addr = self.reg(src).wrapping_add(offset as i64 as u64);
        let loaded = context.load_memory(addr, width);
        self.set_reg(dst, loaded);
        Ok(())
    }

    #[inline(always)]
    fn load64(
        &mut self,
        _context: &mut B,
        dst: Register,
        src: u8,
        lower: u32,
    ) -> Result<(), VerifierErrorKind> {
        if src != 0 {
            return Err(VerifierErrorKind::UnsupportedLddwSource(src));
        }
        let value = (lower as u64) | (((self.next_imm as u32) as u64) << 32);
        self.set_reg(dst, value);
        self.advance_pc(1);
        Ok(())
    }

    #[inline(always)]
    fn load_from_packet(
        &mut self,
        context: &mut B,
        dst_reg: Register,
        src_reg: Register,
        offset: i32,
        register_offset: Option<Register>,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let base: i32 = match register_offset {
            Some(r) => self.reg(r) as i32,
            None => 0,
        };
        let Some(total_offset) = base.checked_add(offset) else {
            self.result = Some(0);
            return Ok(());
        };
        let packet_ptr = self.reg(src_reg);
        match context.load_from_packet(packet_ptr, total_offset, width) {
            Some(value) => self.set_reg(dst_reg, value),
            None => self.result = Some(0),
        }
        Ok(())
    }

    #[inline(always)]
    fn store(
        &mut self,
        context: &mut B,
        dst: Register,
        offset: i16,
        src: Source,
        width: DataWidth,
    ) -> Result<(), VerifierErrorKind> {
        let value = self.compute_source(src);
        let addr = self.reg(dst).wrapping_add(offset as i64 as u64);
        context.store_memory(addr, value, width);
        Ok(())
    }
}
