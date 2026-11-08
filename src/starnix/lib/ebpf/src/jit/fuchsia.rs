// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{JitFn, compile};
use crate::{BpfValue, EbpfError, EbpfInstruction, ProgramLinkageInfo};

/// Owns a read+execute mapping of compiled native machine code in the current process's root VMAR,
/// unmapping the region when dropped.
#[derive(Debug)]
pub(crate) struct ExecutableMemory {
    addr: usize,
    size: usize,
}

impl ExecutableMemory {
    /// Maps a pre-compiled executable VMO read+execute into the current process's root VMAR and
    /// flushes the data and instruction caches.
    pub(crate) fn from_vmo(compiled_vmo: zx::Vmo) -> Result<Self, EbpfError> {
        let vmo_size = compiled_vmo.get_size().map_err(|s| {
            EbpfError::ProgramLinkError(format!("Failed to get JIT VMO size: {s:?}"))
        })?;
        let size = usize::try_from(vmo_size).map_err(|_| {
            EbpfError::ProgramLinkError(format!("JIT VMO size {vmo_size} does not fit in usize"))
        })?;
        if size == 0 {
            return Err(EbpfError::ProgramLinkError("JIT VMO size must be non-zero".to_string()));
        }

        // `compiled_vmo` contains the executable native machine code produced by the
        // `ProgramLinker` service. Map it read+execute into the current process's root VMAR and
        // flush the CPU data and instruction caches so the newly mapped code is coherent for
        // execution.
        let addr = fuchsia_runtime::vmar_root_self()
            .map(0, &compiled_vmo, 0, size, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_EXECUTE)
            .map_err(|s| EbpfError::ProgramLinkError(format!("Failed to map JIT VMO: {s:?}")))?;
        let memory = Self { addr, size };

        // SAFETY: `[addr .. addr + size]` was just mapped by `vmar_root_self().map` above and
        // is valid for cache flushing in the current address space. If flushing fails, `memory`
        // unmaps the VMAR region on drop.
        zx::Status::ok(unsafe {
            zx::sys::zx_cache_flush(
                addr as *const u8,
                size,
                zx::sys::ZX_CACHE_FLUSH_DATA | zx::sys::ZX_CACHE_FLUSH_INSN,
            )
        })
        .map_err(|s| {
            EbpfError::ProgramLinkError(format!("Failed to flush JIT VMO cache: {s:?}"))
        })?;

        Ok(memory)
    }

    /// Executes the mapped native function with `run_context` and up to 5 `BpfValue` arguments.
    ///
    /// # Safety
    /// `self.addr` must point to a valid, mapped, executable JIT function matching the `JitFn`
    /// ABI for `run_context` and `args`.
    #[inline(always)]
    pub(crate) unsafe fn execute(&self, run_context: *mut (), args: &[BpfValue]) -> u64 {
        #[allow(clippy::get_first)]
        let arg1 = args.get(0).copied().unwrap_or_default().as_u64();
        let arg2 = args.get(1).copied().unwrap_or_default().as_u64();
        let arg3 = args.get(2).copied().unwrap_or_default().as_u64();
        let arg4 = args.get(3).copied().unwrap_or_default().as_u64();
        let arg5 = args.get(4).copied().unwrap_or_default().as_u64();
        // SAFETY: The caller guarantees `self.addr` is the entry point of a mapped `JitFn` and
        // that `run_context` and `arg1..arg5` satisfy its ABI and safety preconditions.
        unsafe {
            let f = std::mem::transmute::<usize, JitFn>(self.addr);
            f(run_context, arg1, arg2, arg3, arg4, arg5)
        }
    }
}

impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        if self.addr != 0 && self.size != 0 {
            // SAFETY: `self.addr` and `self.size` were returned by `vmar_root_self().map` in
            // `ExecutableMemory::from_vmo` and are only unmapped once here on drop while no thread
            // is executing the program (as `&mut self` proves unique ownership).
            unsafe {
                let _ = fuchsia_runtime::vmar_root_self().unmap(self.addr, self.size);
            }
        }
    }
}

/// Compiles relocated eBPF instructions into a read+execute Zircon VMO containing native
/// machine code.
pub fn compile_jit_vmo(
    code: &[EbpfInstruction],
    linkage: &ProgramLinkageInfo,
    vmex_resource: &zx::Resource,
) -> Result<zx::Vmo, EbpfError> {
    let machine_code = compile(code, &linkage.helpers, linkage.packet_load_fn_addr)?;

    let size = machine_code.len() as u64;
    let vmo = zx::Vmo::create(size)
        .map_err(|s| EbpfError::ProgramLinkError(format!("Failed to create JIT VMO: {s:?}")))?;
    vmo.write(&machine_code, 0).map_err(|s| {
        EbpfError::ProgramLinkError(format!("Failed to write JIT machine code to VMO: {s:?}"))
    })?;
    vmo.op_range(zx::VmoOp::CACHE_SYNC, 0, size)
        .map_err(|s| EbpfError::ProgramLinkError(format!("Failed to sync JIT VMO cache: {s:?}")))?;

    let vmo = vmo.replace_as_executable(vmex_resource).map_err(|s| {
        EbpfError::ProgramLinkError(format!("Failed to mark JIT VMO executable: {s:?}"))
    })?;
    let rights = zx::Rights::READ
        | zx::Rights::EXECUTE
        | zx::Rights::MAP
        | zx::Rights::TRANSFER
        | zx::Rights::DUPLICATE
        | zx::Rights::GET_PROPERTY;
    vmo.replace_handle(rights).map_err(|s| {
        EbpfError::ProgramLinkError(format!("Failed to restrict JIT VMO rights: {s:?}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JitError;
    use crate::conformance::test::parse_asm;

    #[test]
    fn test_executable_memory_rejects_invalid_vmo() {
        let invalid_vmo = zx::Vmo::from(zx::NullableHandle::invalid());
        let err = ExecutableMemory::from_vmo(invalid_vmo)
            .expect_err("invalid VMO handle must fail get_size");
        assert!(matches!(err, EbpfError::ProgramLinkError(msg) if msg.contains("JIT VMO size")));
    }

    #[test]
    fn test_executable_memory_rejects_zero_size_vmo() {
        let empty_vmo = zx::Vmo::create(0).expect("create zero-sized VMO");
        let err =
            ExecutableMemory::from_vmo(empty_vmo).expect_err("zero-sized VMO must be rejected");
        assert_eq!(err, EbpfError::ProgramLinkError("JIT VMO size must be non-zero".to_string()),);
    }

    #[test]
    fn test_executable_memory_rejects_non_executable_vmo() {
        let non_exec_vmo = zx::Vmo::create(4096).expect("create non-executable VMO");
        let err = ExecutableMemory::from_vmo(non_exec_vmo)
            .expect_err("VMO without zx::Rights::EXECUTE must fail VMAR mapping");
        assert!(
            matches!(err, EbpfError::ProgramLinkError(msg) if msg.contains("Failed to map JIT VMO"))
        );
    }

    #[test]
    fn test_compile_jit_vmo_error_paths() {
        let code = parse_asm(
            r#"
            mov %r0, 0
            exit
            "#,
        );
        let linkage = ProgramLinkageInfo::default();
        let invalid_vmex = zx::Resource::from(zx::NullableHandle::invalid());
        let err = compile_jit_vmo(&code, &linkage, &invalid_vmex).expect_err(
            "compile_jit_vmo without supported backend or valid VmexResource must fail",
        );
        if !super::super::IS_SUPPORTED {
            assert_eq!(err, EbpfError::from(JitError::UnsupportedArchitecture));
        } else {
            let EbpfError::ProgramLinkError(msg) = err else {
                panic!("unexpected error: {err:?}");
            };
            assert!(msg.contains("Failed to mark JIT VMO executable"), "unexpected message: {msg}");
        }
    }
}
