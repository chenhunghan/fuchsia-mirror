// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/annotations/encode.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <fidl/fuchsia.feedback/cpp/natural_ostream.h>  // IWYU pragma: keep

#include <string>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/annotations/types.h"
#include "src/developer/forensics/utils/errors.h"

namespace forensics::feedback {
namespace {

using ::testing::UnorderedElementsAreArray;

TEST(EncodeTest, AnnotationsAsFidl) {
  const auto annotations = Encode<fuchsia_feedback::Annotations>(Annotations({
      {"key1", ErrorOrString("value1")},
      {"key2", ErrorOrString("value2")},
      {"key3", ErrorOrString(Error::kTimeout)},
  }));

  const std::vector<fuchsia_feedback::Annotation> expected({
      fuchsia_feedback::Annotation{{.key = "key1", .value = "value1"}},
      fuchsia_feedback::Annotation{{.key = "key2", .value = "value2"}},
  });

  ASSERT_TRUE(annotations.annotations2().has_value());
  EXPECT_EQ(*annotations.annotations2(), expected);
}

TEST(EncodeTest, EmptyAnnotationsAsFidl) {
  const auto annotations = Encode<fuchsia_feedback::Annotations>(Annotations({}));

  EXPECT_FALSE(annotations.annotations2().has_value());
}

TEST(EncodeTest, AnnotationsAsFidlLargeSize) {
  Annotations annotations;
  std::vector<fuchsia_feedback::Annotation> expected;

  for (uint32_t i = 0; i < fuchsia_feedback::kMaxNumAnnotations2Provided; ++i) {
    const std::string key = "key" + std::to_string(i);
    annotations.insert({key, ErrorOrString("fake_value")});
    expected.push_back(fuchsia_feedback::Annotation{{.key = key, .value = "fake_value"}});
  }

  const auto fidl_annotations = Encode<fuchsia_feedback::Annotations>(annotations);

  ASSERT_TRUE(fidl_annotations.annotations2().has_value());
  EXPECT_THAT(*fidl_annotations.annotations2(), UnorderedElementsAreArray(expected));
}

TEST(EncodeTest, AnnotationsAsString) {
  EXPECT_EQ(Encode<std::string>(Annotations({
                {"key1", ErrorOrString("value1")},
                {"key2", ErrorOrString("value2")},
                {"key3", ErrorOrString(Error::kTimeout)},
            })),
            R"({
    "key1": "value1",
    "key2": "value2"
})");
}

TEST(EncodeTest, EmptyAnnotationsAsString) {
  EXPECT_EQ(Encode<std::string>(Annotations({})), "{}");
}

}  // namespace
}  // namespace forensics::feedback
