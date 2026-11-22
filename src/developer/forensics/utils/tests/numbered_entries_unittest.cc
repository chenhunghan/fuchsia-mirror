// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/utils/numbered_entries.h"

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"

namespace forensics {
namespace {

using ::testing::ElementsAre;
using ::testing::IsEmpty;

class NumberedEntriesTest : public ::testing::Test {
 protected:
  void WriteFile(const std::string& name) {
    ASSERT_TRUE(files::WriteFile(files::JoinPath(temp_dir_.path(), name), "content"));
  }

  void CreateDirectory(const std::string& name) {
    ASSERT_TRUE(files::CreateDirectory(files::JoinPath(temp_dir_.path(), name)));
  }

  const std::string& Dir() { return temp_dir_.path(); }

 private:
  files::ScopedTempDir temp_dir_;
};

TEST_F(NumberedEntriesTest, SortsNumerically) {
  WriteFile("10");
  WriteFile("2");
  WriteFile("0");

  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kFile), ElementsAre(0u, 2u, 10u));
}

TEST_F(NumberedEntriesTest, SkipsNonNumericNames) {
  WriteFile("1");
  WriteFile("boot_info.json");
  WriteFile("-1");
  WriteFile("+2");
  WriteFile("1a");
  WriteFile("18446744073709551616");

  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kFile), ElementsAre(1u));
}

TEST_F(NumberedEntriesTest, SkipsNamesWithLeadingZeros) {
  WriteFile("1");
  WriteFile("01");
  WriteFile("00");

  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kFile), ElementsAre(1u));
}

TEST_F(NumberedEntriesTest, SkipsDirectoryNamesWithLeadingZeros) {
  CreateDirectory("1");
  CreateDirectory("01");

  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kDirectory), ElementsAre(1u));
}

TEST_F(NumberedEntriesTest, FiltersByType) {
  WriteFile("1");
  CreateDirectory("2");

  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kFile), ElementsAre(1u));
  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kDirectory), ElementsAre(2u));
}

TEST_F(NumberedEntriesTest, EmptyDirectory) {
  EXPECT_THAT(GetNumberedEntries(Dir(), EntryType::kFile), IsEmpty());
}

TEST_F(NumberedEntriesTest, MissingDirectory) {
  EXPECT_THAT(GetNumberedEntries(files::JoinPath(Dir(), "missing"), EntryType::kFile), IsEmpty());
}

}  // namespace
}  // namespace forensics
