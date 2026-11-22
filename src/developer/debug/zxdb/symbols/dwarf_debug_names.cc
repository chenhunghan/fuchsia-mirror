// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/symbols/dwarf_debug_names.h"

#include <cstdint>
#include <cstring>
#include <optional>
#include <set>
#include <span>
#include <string>

#include <safemath/checked_math.h>

#include "llvm/DebugInfo/DWARF/DWARFContext.h"
#include "llvm/Support/Error.h"
#include "src/developer/debug/zxdb/symbols/dwarf_binary.h"

namespace zxdb {

namespace {

// Returns true when a variable entry in .debug_names has a parent index entry inside a function
// or lexical block. Skipping function-local variables avoids extracting compilation units during
// module-wide variable lookups.
bool IsLocalVariableEntry(const llvm::DWARFDebugNames::Entry& entry) {
  if (entry.tag() != llvm::dwarf::DW_TAG_variable || !entry.hasParentInformation()) {
    return false;
  }

  llvm::Expected<std::optional<llvm::DWARFDebugNames::Entry>> parent_or_err =
      entry.getParentDIEEntry();
  if (!parent_or_err) {
    llvm::consumeError(parent_or_err.takeError());
    return false;
  }
  const std::optional<llvm::DWARFDebugNames::Entry>& parent = *parent_or_err;
  if (!parent.has_value()) {
    return false;
  }

  llvm::dwarf::Tag parent_tag = parent->tag();
  return parent_tag == llvm::dwarf::DW_TAG_subprogram ||
         parent_tag == llvm::dwarf::DW_TAG_inlined_subroutine ||
         parent_tag == llvm::dwarf::DW_TAG_lexical_block;
}

std::optional<DwarfDieRef> ResolveEntryDieRef(const llvm::DWARFDebugNames::Entry& entry) {
  if (IsLocalVariableEntry(entry)) {
    return std::nullopt;
  }

  const std::optional<uint64_t> die_unit_offset = entry.getDIEUnitOffset();

  // Check compilation unit entries first. Both the compile unit offset and DIE unit offset
  // must be present. Offset 0 is a valid section offset in DWARF.
  const std::optional<uint64_t> cu_offset = entry.getCUOffset();
  if (cu_offset.has_value() && die_unit_offset.has_value()) {
    uint64_t abs_offset = 0;
    if (safemath::CheckAdd(*cu_offset, *die_unit_offset).AssignIfValid(&abs_offset)) {
      return DwarfDieRef::Main(abs_offset);
    }
    return std::nullopt;
  }

  // Handle local type unit entries located in .debug_info for DWARF 5.
  const std::optional<uint64_t> local_tu_offset = entry.getLocalTUOffset();
  if (local_tu_offset.has_value() && die_unit_offset.has_value()) {
    uint64_t abs_offset = 0;
    if (safemath::CheckAdd(*local_tu_offset, *die_unit_offset).AssignIfValid(&abs_offset)) {
      return DwarfDieRef::Main(abs_offset);
    }
    return std::nullopt;
  }

  // TODO(https://fxbug.dev/583730372): Resolve foreign type unit entries once zxdb supports
  // split DWARF and tracks owning DWO binaries on DebugNamesEntry.
  return std::nullopt;
}

void AppendEntriesForMatchedName(const llvm::DWARFDebugNames& debug_names, const char* str,
                                 std::set<DwarfDieRef>* seen, std::set<std::string>* seen_names,
                                 std::vector<DebugNamesEntry>* results) {
  if (!seen_names->insert(str).second) {
    return;
  }
  for (const auto& entry : FindDebugNamesEntries(debug_names, str)) {
    if (seen->insert(entry.die_ref).second) {
      results->push_back(entry);
    }
  }
}

// Checks whether the .debug_str entry at str_off starts with prefix without scanning for its NUL
// terminator first. Slicing prefix.size() bytes within str_data is safe even when the entry is
// shorter than prefix because a non-NUL prefix won't match across the entry's NUL byte.
bool CandidateMatchesPrefix(std::span<const char> str_data, uint32_t str_off,
                            std::string_view prefix) {
  if (prefix.empty()) {
    return false;
  }
  size_t end_off = 0;
  if (!safemath::CheckAdd(str_off, prefix.size()).AssignIfValid(&end_off) ||
      end_off > str_data.size()) {
    return false;
  }
  std::span<const char> candidate = str_data.subspan(str_off, prefix.size());
  if (candidate.back() != prefix.back() || candidate.front() != prefix.front()) {
    return false;
  }
  return std::memcmp(candidate.data(), prefix.data(), candidate.size()) == 0;
}

// Scans the DWARF32 string offset array directly against the .debug_str buffer to avoid computing
// string lengths for every entry in large .debug_names tables.
bool TryScanNameIndexPrefixFast(const llvm::DWARFDebugNames& debug_names, const DwarfBinary& binary,
                                const llvm::DWARFDebugNames::NameIndex& name_index,
                                std::string_view prefix, std::set<DwarfDieRef>* seen,
                                std::set<std::string>* seen_names,
                                std::vector<DebugNamesEntry>* results) {
  llvm::DWARFContext* context = binary.GetLLVMContext();
  if (!context || prefix.empty() || prefix.contains('\0')) {
    return false;
  }

  llvm::DWARFDebugNames::Header hdr = name_index.getHeader();
  if (hdr.Format != llvm::dwarf::DWARF32) {
    return false;
  }

  llvm::StringRef names_sec = context->getDWARFObj().getNamesSection().Data;
  llvm::StringRef str_sec = context->getDWARFObj().getStrSection();
  std::span<const char> names_data(names_sec.data(), names_sec.size());
  std::span<const char> str_data(str_sec.data(), str_sec.size());

  uint64_t str_offsets_base = name_index.getOffsets().StringOffsetsBase;
  uint64_t table_bytes = static_cast<uint64_t>(hdr.NameCount) * sizeof(uint32_t);
  uint64_t table_end = 0;
  if (!safemath::CheckAdd(str_offsets_base, table_bytes).AssignIfValid(&table_end) ||
      table_end > names_data.size()) {
    return false;
  }

  std::span<const char> offsets_table =
      names_data.subspan(static_cast<size_t>(str_offsets_base), static_cast<size_t>(table_bytes));
  for (uint32_t i = 0; i < hdr.NameCount; i++) {
    uint32_t str_off = 0;
    std::span<const char> entry_bytes =
        offsets_table.subspan(static_cast<size_t>(i) * sizeof(uint32_t), sizeof(uint32_t));
    std::memcpy(&str_off, entry_bytes.data(), sizeof(uint32_t));
    if (!CandidateMatchesPrefix(str_data, str_off, prefix)) {
      continue;
    }

    if (const char* str = name_index.getNameTableEntry(i + 1).getString()) {
      AppendEntriesForMatchedName(debug_names, str, seen, seen_names, results);
    }
  }
  return true;
}

}  // namespace

std::vector<DebugNamesEntry> FindDebugNamesEntries(const llvm::DWARFDebugNames& debug_names,
                                                   std::string_view name) {
  std::vector<DebugNamesEntry> results;
  std::set<DwarfDieRef> seen;

  llvm::StringRef key(name.data(), name.size());
  for (const auto& entry : debug_names.equal_range(key)) {
    std::optional<DwarfDieRef> die_ref = ResolveEntryDieRef(entry);
    if (!die_ref.has_value() || !die_ref->is_valid()) {
      continue;
    }

    if (seen.insert(*die_ref).second) {
      results.push_back({.die_ref = *die_ref, .tag = entry.tag()});
    }
  }

  return results;
}

std::vector<DebugNamesEntry> FindDebugNamesPrefixEntries(const llvm::DWARFDebugNames& debug_names,
                                                         const DwarfBinary& binary,
                                                         std::string_view prefix) {
  if (prefix.empty() || prefix.contains('\0')) {
    return {};
  }

  std::vector<DebugNamesEntry> results;
  std::set<DwarfDieRef> seen;
  std::set<std::string> seen_names;

  for (const auto& name_index : debug_names) {
    if (TryScanNameIndexPrefixFast(debug_names, binary, name_index, prefix, &seen, &seen_names,
                                   &results)) {
      continue;
    }

    for (const auto& name_entry : name_index) {
      const char* str = name_entry.getString();
      if (!str || std::strncmp(str, prefix.data(), prefix.size()) != 0) {
        continue;
      }

      AppendEntriesForMatchedName(debug_names, str, &seen, &seen_names, &results);
    }
  }

  return results;
}

}  // namespace zxdb
