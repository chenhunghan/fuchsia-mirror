// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_DWARF_DEBUG_NAMES_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_DWARF_DEBUG_NAMES_H_

#include <string_view>
#include <vector>

#include "llvm/BinaryFormat/Dwarf.h"
#include "llvm/DebugInfo/DWARF/DWARFAcceleratorTable.h"
#include "src/developer/debug/zxdb/symbols/dwarf_die_ref.h"

namespace zxdb {

class DwarfBinary;

// Represents a matching entry resolved from the DWARF v5 .debug_names accelerator table.
struct DebugNamesEntry {
  DwarfDieRef die_ref;
  llvm::dwarf::Tag tag = llvm::dwarf::DW_TAG_null;

  auto operator<=>(const DebugNamesEntry& other) const = default;
};

// Looks up all entries in the DWARF v5 accelerator table matching name.
//
// Computes absolute DIE offsets from compile unit or local type unit offsets, safely handling
// unit offset 0.
std::vector<DebugNamesEntry> FindDebugNamesEntries(const llvm::DWARFDebugNames& debug_names,
                                                   std::string_view name);

// Looks up all entries whose name starts with prefix in the DWARF v5 accelerator table.
//
// Iterates the name table entries across all name indexes in the section.
std::vector<DebugNamesEntry> FindDebugNamesPrefixEntries(const llvm::DWARFDebugNames& debug_names,
                                                         const DwarfBinary& binary,
                                                         std::string_view prefix);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_SYMBOLS_DWARF_DEBUG_NAMES_H_
