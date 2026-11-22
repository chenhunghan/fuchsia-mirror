// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DIAGNOSTICS_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DIAGNOSTICS_H_

#include <zircon/types.h>

class ProcessDispatcher;
class VmObject;

extern "C" {
void cpp_dump_all_vm_objects(bool hidden_only, char format_unit);
void cpp_dump_orphaned_slices(char format_unit);
void cpp_dump_orphaned_mapped_vmos(char format_unit);
void cpp_dump_process_vm_objects(zx_koid_t id, char format_unit);
void cpp_dump_vm_object_cow_tree(zx_koid_t id);
void cpp_dump_vm_object(const VmObject& vmo, char format_unit, zx_handle_t handle,
                        zx_rights_t rights, zx_koid_t koid);
void rust_diagnostics_dump_aspace_vm_objects(const ProcessDispatcher& process, char format_unit);
}  // extern "C"

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_DIAGNOSTICS_H_
