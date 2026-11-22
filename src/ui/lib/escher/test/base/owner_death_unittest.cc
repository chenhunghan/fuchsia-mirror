// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <memory>

#include <gtest/gtest.h>

#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/ui/lib/escher/base/make.h"
#include "src/ui/lib/escher/base/ownable.h"
#include "src/ui/lib/escher/base/owner.h"
#include "src/ui/lib/escher/base/type_info.h"

// This test intentionally aborts, so it lives in `escher_fatal_unittests`, whose package allows
// FATAL logs and whose component manifest uses `fuchsia.process.Launcher` to spawn death-test child
// processes.

namespace {

enum class DeathTestOwnableTypes {
  kDeathTestOwnable = 1,
};

using DeathTestOwnableTypeInfo = escher::TypeInfo<DeathTestOwnableTypes>;

class DeathTestOwnable : public escher::Ownable<DeathTestOwnable, DeathTestOwnableTypeInfo> {
 public:
  static const TypeInfo kTypeInfo;
  const TypeInfo& type_info() const override { return kTypeInfo; }
};

const DeathTestOwnableTypeInfo DeathTestOwnable::kTypeInfo(
    "DeathTestOwnable", DeathTestOwnableTypes::kDeathTestOwnable);

class DeathTestOwner : public escher::Owner<DeathTestOwnable, DeathTestOwnableTypeInfo> {
 public:
  // `OnReceiveOwnable()` moves released ownables into `parked`, which may outlive this owner,
  // without relinquishing ownership.
  explicit DeathTestOwner(std::unique_ptr<DeathTestOwnable>* parked) : parked_(parked) {}

  fxl::RefPtr<DeathTestOwnable> NewOwnable() {
    auto ownable = escher::Make<DeathTestOwnable>();
    BecomeOwnerOf(ownable.get());
    return ownable;
  }

 private:
  void OnReceiveOwnable(std::unique_ptr<DeathTestOwnable> unreffed) override {
    *parked_ = std::move(unreffed);
  }

  std::unique_ptr<DeathTestOwnable>* const parked_;
};

// Both outstanding `Ownable`s keep a raw pointer to the owner: the live one calls
// `OnReceiveOwnable()` on it when released, and the parked one calls `DecrementOwnableCount()` on
// it when destroyed.
TEST(OwnerDeathTest, DestroyingOwnerWithOutstandingOwnablesAborts) {
  EXPECT_DEATH(
      {
        std::unique_ptr<DeathTestOwnable> parked;
        auto owner = std::make_unique<DeathTestOwner>(&parked);
        {
          // Dropping the only reference parks this ownable at ref-count zero.
          auto released = owner->NewOwnable();
        }
        auto live = owner->NewOwnable();
        owner.reset();
      },
      "Owner destroyed while 2 Ownables still hold a raw pointer to it");
}

}  // namespace
