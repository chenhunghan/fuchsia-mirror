// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/symbols/dwarf_debug_names.h"

#include <string>

#include <gtest/gtest.h>

#include "llvm/DebugInfo/DWARF/DWARFContext.h"
#include "src/developer/debug/zxdb/symbols/dwarf_binary_impl.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/input_location.h"
#include "src/developer/debug/zxdb/symbols/module_symbols_impl.h"
#include "src/developer/debug/zxdb/symbols/test_symbol_module.h"
#include "src/developer/debug/zxdb/symbols/variable.h"

namespace zxdb {

namespace {

TEST(DwarfDebugNamesTest, LookupBuiltBinary) {
  TestSymbolModule setup(TestSymbolModule::kBuilt);
  ASSERT_TRUE(setup.Init().ok());

  ModuleSymbolsImpl* module = setup.symbols();
  ASSERT_TRUE(module);
  EXPECT_TRUE(module->HasDebugNames());

  const llvm::DWARFDebugNames* debug_names = module->GetDebugNames();
  ASSERT_TRUE(debug_names);

  // Look up a known global function by unqualified name.
  auto fn_entries = FindDebugNamesEntries(*debug_names, TestSymbolModule::kMyFunctionName);
  ASSERT_FALSE(fn_entries.empty());
  bool found_func = false;
  for (const auto& entry : fn_entries) {
    if (entry.tag == llvm::dwarf::DW_TAG_subprogram) {
      found_func = true;
      LazySymbol lazy = module->GetSymbolFactory()->MakeLazy(entry.die_ref);
      const Symbol* sym = lazy.Get();
      ASSERT_TRUE(sym);
      const Function* func = sym->As<Function>();
      ASSERT_TRUE(func);
      EXPECT_EQ(func->GetAssignedName(), TestSymbolModule::kMyFunctionName);
    }
  }
  EXPECT_TRUE(found_func);

  // Look up a class structure type by short name. DWARF accelerator tables index symbols by
  // their short (leaf) names.
  auto class_entries = FindDebugNamesEntries(*debug_names, "MyClass");
  ASSERT_FALSE(class_entries.empty());
  bool found_class = false;
  for (const auto& entry : class_entries) {
    if (entry.tag == llvm::dwarf::DW_TAG_structure_type ||
        entry.tag == llvm::dwarf::DW_TAG_class_type) {
      found_class = true;
      LazySymbol lazy = module->GetSymbolFactory()->MakeLazy(entry.die_ref);
      const Symbol* sym = lazy.Get();
      ASSERT_TRUE(sym);
      EXPECT_EQ(sym->GetAssignedName(), "MyClass");
      EXPECT_EQ(sym->GetFullName(), TestSymbolModule::kMyClassName);
    }
  }
  EXPECT_TRUE(found_class);

  // Look up a namespace.
  auto ns_entries = FindDebugNamesEntries(*debug_names, TestSymbolModule::kMyNamespaceName);
  ASSERT_FALSE(ns_entries.empty());
  bool found_ns = false;
  for (const auto& entry : ns_entries) {
    if (entry.tag == llvm::dwarf::DW_TAG_namespace) {
      found_ns = true;
      LazySymbol lazy = module->GetSymbolFactory()->MakeLazy(entry.die_ref);
      const Symbol* sym = lazy.Get();
      ASSERT_TRUE(sym);
      EXPECT_EQ(sym->GetAssignedName(), TestSymbolModule::kMyNamespaceName);
    }
  }
  EXPECT_TRUE(found_ns);

  // Look up a global variable by short name.
  auto var_entries = FindDebugNamesEntries(*debug_names, "kGlobal");
  ASSERT_FALSE(var_entries.empty());
  bool found_var = false;
  for (const auto& entry : var_entries) {
    if (entry.tag == llvm::dwarf::DW_TAG_variable) {
      found_var = true;
      LazySymbol lazy = module->GetSymbolFactory()->MakeLazy(entry.die_ref);
      const Symbol* sym = lazy.Get();
      ASSERT_TRUE(sym);
      const Variable* var = sym->As<Variable>();
      ASSERT_TRUE(var);
      EXPECT_EQ(var->GetAssignedName(), "kGlobal");
      EXPECT_EQ(var->GetFullName(), TestSymbolModule::kGlobalName);
    }
  }
  EXPECT_TRUE(found_var);

  // Function-local variables are filtered out via DW_IDX_parent inspection.
  EXPECT_TRUE(FindDebugNamesEntries(*debug_names, "my_class").empty());
  EXPECT_TRUE(FindDebugNamesEntries(*debug_names, "var1").empty());
  EXPECT_TRUE(FindDebugNamesEntries(*debug_names, "var2").empty());

  // Querying a nonexistent symbol should yield no results.
  auto missing = FindDebugNamesEntries(*debug_names, "NonexistentSymbolNameThatShouldNeverMatch");
  EXPECT_TRUE(missing.empty());

  // Prefix searches find symbols beginning with the prefix string.
  auto prefix_entries = FindDebugNamesPrefixEntries(*debug_names, *module->binary(), "My");
  EXPECT_FALSE(prefix_entries.empty());

  // Single-character, embedded-NUL, and oversized prefixes stay within .debug_str bounds.
  EXPECT_FALSE(FindDebugNamesPrefixEntries(*debug_names, *module->binary(), "M").empty());
  EXPECT_TRUE(FindDebugNamesPrefixEntries(*debug_names, *module->binary(),
                                          std::string_view("MyFunction\0extra", 16))
                  .empty());
  std::string long_symbol_prefix =
      std::string(TestSymbolModule::kMyFunctionName) + std::string(4096, 'x');
  EXPECT_TRUE(
      FindDebugNamesPrefixEntries(*debug_names, *module->binary(), long_symbol_prefix).empty());
  ASSERT_TRUE(module->binary()->GetLLVMContext());
  size_t str_sec_size = module->binary()->GetLLVMContext()->getDWARFObj().getStrSection().size();
  std::string oversized_prefix(str_sec_size + 1024, 'M');
  EXPECT_TRUE(
      FindDebugNamesPrefixEntries(*debug_names, *module->binary(), oversized_prefix).empty());

  // Empty prefix queries return empty without out-of-bounds access.
  EXPECT_TRUE(FindDebugNamesPrefixEntries(*debug_names, *module->binary(), "").empty());
  EXPECT_TRUE(
      FindDebugNamesPrefixEntries(*debug_names, *module->binary(), std::string_view()).empty());

  // Resolving symbol input locations functions via .debug_names lookup.
  SymbolContext symbol_context = SymbolContext::ForRelativeAddresses();
  InputLocation input_loc((Identifier(TestSymbolModule::kMyFunctionName)));
  auto locs = module->ResolveInputLocation(symbol_context, input_loc);
  EXPECT_FALSE(locs.empty());

  // Empty and global-only identifiers return empty without crashing or undefined behavior.
  InputLocation empty_loc((Identifier()));
  EXPECT_TRUE(module->ResolveInputLocation(symbol_context, empty_loc).empty());
  InputLocation global_only_loc((Identifier(IdentifierQualification::kGlobal)));
  EXPECT_TRUE(module->ResolveInputLocation(symbol_context, global_only_loc).empty());

  // Unqualified lookups do not suffix-match namespaced or class member functions, while
  // fully-qualified lookups succeed.
  InputLocation unqual_ns_loc((Identifier(IdentifierComponent("NamespaceFunction"))));
  EXPECT_TRUE(module->ResolveInputLocation(symbol_context, unqual_ns_loc).empty());
  InputLocation unqual_member_loc((Identifier(IdentifierComponent("MyMemberOne"))));
  EXPECT_TRUE(module->ResolveInputLocation(symbol_context, unqual_member_loc).empty());
  InputLocation qual_ns_loc(TestSymbolModule::SplitName(TestSymbolModule::kNamespaceFunctionName));
  EXPECT_FALSE(module->ResolveInputLocation(symbol_context, qual_ns_loc).empty());

  // Template function lookup succeeds even when -gsimple-template-names indexes only the base name.
  InputLocation template_loc((Identifier(IdentifierComponent("LineLookupTest<0>"))));
  EXPECT_FALSE(module->ResolveInputLocation(symbol_context, template_loc).empty());

  // Anonymous namespace functions resolve through .debug_names, both unqualified and when
  // round-tripping the symbol's full identifier or explicit $anon qualification.
  InputLocation anon_loc((Identifier(TestSymbolModule::kAnonNSFunctionName)));
  auto anon_locs = module->ResolveInputLocation(symbol_context, anon_loc);
  ASSERT_FALSE(anon_locs.empty());
  InputLocation anon_roundtrip_loc(anon_locs[0].symbol().Get()->GetIdentifier());
  EXPECT_FALSE(module->ResolveInputLocation(symbol_context, anon_roundtrip_loc).empty());
  Identifier explicit_anon_id{IdentifierComponent(SpecialIdentifier::kAnon)};
  explicit_anon_id.AppendComponent(IdentifierComponent(TestSymbolModule::kAnonNSFunctionName));
  EXPECT_FALSE(
      module->ResolveInputLocation(symbol_context, InputLocation(explicit_anon_id)).empty());

  // When .debug_names is active, the full DIE index is skipped while GetStatus() still reports
  // indexed files and symbol counts, and line-table-only file matching and breakpoint resolution
  // succeed before any full DIE scan runs.
  EXPECT_EQ(module->GetIndex().CountSymbolsIndexed(), 0u);
  ModuleSymbolStatus status = module->GetStatus();
  EXPECT_GT(status.files_indexed, 0u);
  EXPECT_GT(status.functions_indexed, 0u);
  auto file_matches = module->FindFileMatches("zxdb_symbol_test.cc");
  EXPECT_FALSE(file_matches.empty());
  InputLocation line_loc(FileLine("zxdb_symbol_test.cc", TestSymbolModule::kMyFunctionLine));
  EXPECT_FALSE(module->ResolveInputLocation(symbol_context, line_loc).empty());

  // Forcing the zxdb index disables both HasDebugNames() and GetDebugNames() and lazily builds
  // the full DIE index so symbol lookups continue to succeed.
  module->SetForceSlowIndex(true);
  EXPECT_FALSE(module->HasDebugNames());
  EXPECT_EQ(module->GetDebugNames(), nullptr);
  EXPECT_GT(module->GetIndex().CountSymbolsIndexed(), 0u);
  EXPECT_GT(module->GetStatus().functions_indexed, 0u);
  EXPECT_FALSE(module->ResolveInputLocation(symbol_context, input_loc).empty());
  module->SetForceSlowIndex(false);
  EXPECT_TRUE(module->HasDebugNames());
  EXPECT_NE(module->GetDebugNames(), nullptr);
  EXPECT_FALSE(module->ResolveInputLocation(symbol_context, input_loc).empty());
}

TEST(DwarfDebugNamesTest, IndexFallbackWhenAbsent) {
  // The checked-in binary lacks .debug_names accelerator tables.
  TestSymbolModule setup(TestSymbolModule::kCheckedIn);
  ASSERT_TRUE(setup.Init().ok());

  ModuleSymbolsImpl* module = setup.symbols();
  ASSERT_TRUE(module);
  EXPECT_FALSE(module->HasDebugNames());
  EXPECT_EQ(module->GetDebugNames(), nullptr);

  // The zxdb index resolves symbols and source files when .debug_names is absent.
  SymbolContext symbol_context = SymbolContext::ForRelativeAddresses();
  InputLocation input_loc((Identifier(TestSymbolModule::kMyFunctionName)));
  auto locs = module->ResolveInputLocation(symbol_context, input_loc);
  EXPECT_FALSE(locs.empty());

  auto file_matches = module->FindFileMatches("zxdb_symbol_test.cc");
  EXPECT_FALSE(file_matches.empty());
}

}  // namespace

}  // namespace zxdb
