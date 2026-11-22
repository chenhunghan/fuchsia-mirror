// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/resource-vote/cpp/vote-set.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>

#include <algorithm>
#include <string>
#include <utility>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace fdf_resource_vote {
namespace {

using ::testing::HasSubstr;

struct LogRecord {
  LogSeverity severity;
  std::string message;
};

class BoolVoteSetTest : public ::testing::Test {
 protected:
  BoolVoteSetTest() : votes_("clk-test") {
    votes_.SetLogSinkForTesting([this](LogSeverity severity, const std::string& message) {
      logs_.push_back({severity, message});
    });
  }

  // Commits a plan that is expected not to change the aggregate.
  void CommitUnchanged(Plan<bool> plan) {
    EXPECT_FALSE(plan.changed());
    votes_.Commit(std::move(plan));
  }

  // Commits a plan that is expected to change the aggregate to `enabled`.
  void CommitChangedTo(Plan<bool> plan, bool enabled) {
    EXPECT_TRUE(plan.changed());
    EXPECT_EQ(plan.new_aggregate().value_or(false), enabled);
    votes_.Commit(std::move(plan));
    EXPECT_EQ(votes_.enabled(), enabled);
  }

  size_t CountLogs(LogSeverity severity) const {
    return static_cast<size_t>(
        std::count_if(logs_.begin(), logs_.end(),
                      [severity](const LogRecord& log) { return log.severity == severity; }));
  }

  BoolVoteSet votes_;
  std::vector<LogRecord> logs_;
};

TEST_F(BoolVoteSetTest, StartsOff) {
  EXPECT_FALSE(votes_.aggregate().has_value());
  EXPECT_FALSE(votes_.enabled());
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.transition_count(), 0u);
  EXPECT_EQ(votes_.resource_name(), "clk-test");
}

TEST_F(BoolVoteSetTest, VoteIdsAreUniqueAndMoveClearsId) {
  Vote a("a", CloseBehavior::kOrphanOnClose);
  Vote b("a", CloseBehavior::kOrphanOnClose);
  EXPECT_NE(a.id(), 0u);
  EXPECT_NE(a.id(), b.id());

  uint64_t id = a.id();
  Vote c(std::move(a));
  EXPECT_EQ(c.id(), id);
  EXPECT_EQ(c.node_key(), "a");
  EXPECT_EQ(c.close_behavior(), CloseBehavior::kOrphanOnClose);
  EXPECT_EQ(a.id(), 0u);  // NOLINT(bugprone-use-after-move)
}

TEST_F(BoolVoteSetTest, CountedEnableDisable) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);

  auto plan = votes_.PrepareEnable(vote);
  EXPECT_FALSE(plan.old_aggregate().has_value());
  ASSERT_TRUE(plan.new_aggregate().has_value());
  EXPECT_TRUE(*plan.new_aggregate());
  CommitChangedTo(std::move(plan), true);

  // Phase 1: a second Enable on the same connection stacks.
  CommitUnchanged(votes_.PrepareEnable(vote));
  ASSERT_EQ(votes_.entries().size(), 1u);
  EXPECT_EQ(votes_.entries()[0].count, 2u);
  EXPECT_EQ(votes_.entries()[0].kind, EntryKind::kLive);
  EXPECT_EQ(votes_.entries()[0].key, "node-a");

  CommitUnchanged(votes_.PrepareDisable(vote));
  EXPECT_TRUE(votes_.enabled());
  EXPECT_EQ(votes_.entries()[0].count, 1u);

  CommitChangedTo(votes_.PrepareDisable(vote), false);
  EXPECT_TRUE(votes_.entries().empty());

  // Disable without a vote saturates at 0 and does not assert.
  CommitUnchanged(votes_.PrepareDisable(vote));
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.transition_count(), 2u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, AggregateOfSeveralVotes) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  Vote b("node-b", CloseBehavior::kOrphanOnClose);

  CommitChangedTo(votes_.PrepareEnable(a), true);
  CommitUnchanged(votes_.PrepareEnable(b));
  CommitUnchanged(votes_.PrepareDisable(a));
  EXPECT_TRUE(votes_.enabled());
  CommitChangedTo(votes_.PrepareDisable(b), false);
}

TEST_F(BoolVoteSetTest, AbandonLeavesStateUnchanged) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);

  auto plan = votes_.PrepareEnable(vote);
  EXPECT_TRUE(plan.changed());
  votes_.Abandon(std::move(plan));
  EXPECT_FALSE(votes_.enabled());
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.transition_count(), 0u);

  CommitChangedTo(votes_.PrepareEnable(vote), true);
  auto disable = votes_.PrepareDisable(vote);
  EXPECT_TRUE(disable.changed());
  votes_.Abandon(std::move(disable));
  EXPECT_TRUE(votes_.enabled());
  ASSERT_EQ(votes_.entries().size(), 1u);
  EXPECT_EQ(votes_.entries()[0].count, 1u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, DroppedPlanLeavesStateUnchanged) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  {
    auto plan = votes_.PrepareEnable(vote);
  }
  EXPECT_FALSE(votes_.enabled());
  // A plan prepared after a dropped plan is valid.
  CommitChangedTo(votes_.PrepareEnable(vote), true);
}

TEST_F(BoolVoteSetTest, PrepareDoesNotMutate) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  auto plan = votes_.PrepareEnable(vote);
  EXPECT_FALSE(votes_.enabled());
  EXPECT_TRUE(votes_.entries().empty());
  votes_.Commit(std::move(plan));
  EXPECT_TRUE(votes_.enabled());
}

TEST_F(BoolVoteSetTest, CloseWithoutRequestIsSilent) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitUnchanged(votes_.PrepareClose(vote));
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.orphan_count(), 0u);
  EXPECT_TRUE(logs_.empty());

  Vote balanced("node-b", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(balanced), true);
  CommitChangedTo(votes_.PrepareDisable(balanced), false);
  CommitUnchanged(votes_.PrepareClose(balanced));
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, OrphanOnCloseHoldsAndWarnsOnce) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(vote), true);
  CommitUnchanged(votes_.PrepareEnable(vote));

  CommitUnchanged(votes_.PrepareClose(vote));
  EXPECT_TRUE(votes_.enabled());
  EXPECT_EQ(votes_.orphan_count(), 1u);
  EXPECT_EQ(votes_.orphaned_total(), 1u);

  // The whole count is orphaned as one entry.
  auto entries = votes_.entries();
  ASSERT_EQ(entries.size(), 1u);
  EXPECT_EQ(entries[0].kind, EntryKind::kOrphan);
  EXPECT_EQ(entries[0].key, "node-a");
  EXPECT_EQ(entries[0].count, 2u);

  ASSERT_EQ(logs_.size(), 1u);
  EXPECT_EQ(logs_[0].severity, LogSeverity::kWarning);
  EXPECT_THAT(logs_[0].message, HasSubstr("clk-test"));
  EXPECT_THAT(logs_[0].message, HasSubstr("node-a"));
  EXPECT_THAT(logs_[0].message, HasSubstr("on"));
}

TEST_F(BoolVoteSetTest, ReclaimOnEnableFromSameNode) {
  Vote old_vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(old_vote), true);
  CommitUnchanged(votes_.PrepareEnable(old_vote));
  CommitUnchanged(votes_.PrepareClose(old_vote));
  ASSERT_EQ(votes_.orphan_count(), 1u);

  // The rebound driver's Enable reclaims the orphan in place: no hardware change.
  Vote new_vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitUnchanged(votes_.PrepareEnable(new_vote));
  EXPECT_EQ(votes_.orphan_count(), 0u);
  auto entries = votes_.entries();
  ASSERT_EQ(entries.size(), 1u);
  EXPECT_EQ(entries[0].kind, EntryKind::kLive);
  EXPECT_EQ(entries[0].count, 1u);

  // The new instance's count is not inflated by the old instance's enables.
  CommitChangedTo(votes_.PrepareDisable(new_vote), false);
  EXPECT_EQ(votes_.transition_count(), 2u);
}

TEST_F(BoolVoteSetTest, ReclaimRemovesAllOrphansOfNodeOnly) {
  Vote a1("node-a", CloseBehavior::kOrphanOnClose);
  Vote a2("node-a", CloseBehavior::kOrphanOnClose);
  Vote b("node-b", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(a1), true);
  CommitUnchanged(votes_.PrepareEnable(a2));
  CommitUnchanged(votes_.PrepareEnable(b));
  CommitUnchanged(votes_.PrepareClose(a1));
  CommitUnchanged(votes_.PrepareClose(a2));
  CommitUnchanged(votes_.PrepareClose(b));
  EXPECT_EQ(votes_.orphan_count(), 3u);
  EXPECT_EQ(CountLogs(LogSeverity::kWarning), 3u);

  Vote a3("node-a", CloseBehavior::kOrphanOnClose);
  CommitUnchanged(votes_.PrepareEnable(a3));
  EXPECT_EQ(votes_.orphan_count(), 1u);
  auto entries = votes_.entries();
  ASSERT_EQ(entries.size(), 2u);
  EXPECT_EQ(entries[0].kind, EntryKind::kOrphan);
  EXPECT_EQ(entries[0].key, "node-b");
  EXPECT_EQ(entries[1].kind, EntryKind::kLive);
  EXPECT_EQ(entries[1].key, "node-a");
}

TEST_F(BoolVoteSetTest, DisableDoesNotReclaim) {
  Vote old_vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(old_vote), true);
  CommitUnchanged(votes_.PrepareClose(old_vote));

  Vote new_vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitUnchanged(votes_.PrepareDisable(new_vote));
  CommitUnchanged(votes_.PrepareClear(new_vote));
  EXPECT_EQ(votes_.orphan_count(), 1u);
  EXPECT_TRUE(votes_.enabled());
}

TEST_F(BoolVoteSetTest, NodeStartedReleasesLeftovers) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  Vote b("node-b", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(a), true);
  CommitUnchanged(votes_.PrepareEnable(b));
  CommitUnchanged(votes_.PrepareClose(a));
  CommitUnchanged(votes_.PrepareClose(b));

  // Unknown node: no-op.
  CommitUnchanged(votes_.PrepareNodeStarted("node-c"));
  EXPECT_EQ(votes_.orphan_count(), 2u);

  CommitUnchanged(votes_.PrepareNodeStarted("node-a"));
  EXPECT_EQ(votes_.orphan_count(), 1u);
  EXPECT_TRUE(votes_.enabled());

  CommitChangedTo(votes_.PrepareNodeStarted("node-b"), false);
  EXPECT_EQ(votes_.orphan_count(), 0u);
  EXPECT_EQ(votes_.orphaned_total(), 2u);
}

TEST_F(BoolVoteSetTest, NodeStartedKeepsLiveVotes) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(a), true);
  CommitUnchanged(votes_.PrepareNodeStarted("node-a"));
  EXPECT_TRUE(votes_.enabled());
}

TEST_F(BoolVoteSetTest, NodeStartedFailureKeepsOrphan) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(a), true);
  CommitUnchanged(votes_.PrepareClose(a));

  auto plan = votes_.PrepareNodeStarted("node-a");
  EXPECT_TRUE(plan.changed());
  votes_.Abandon(std::move(plan));
  EXPECT_EQ(votes_.orphan_count(), 1u);
  EXPECT_TRUE(votes_.enabled());

  // Retried on the next event.
  CommitChangedTo(votes_.PrepareNodeStarted("node-a"), false);
}

TEST_F(BoolVoteSetTest, ReleaseOnClose) {
  Vote debug("debug", CloseBehavior::kReleaseOnClose);
  CommitChangedTo(votes_.PrepareEnable(debug), true);
  CommitUnchanged(votes_.PrepareEnable(debug));
  CommitChangedTo(votes_.PrepareClose(debug), false);
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.orphaned_total(), 0u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, ReleaseOnCloseWithOtherVoter) {
  Vote driver("node-a", CloseBehavior::kOrphanOnClose);
  Vote debug("debug", CloseBehavior::kReleaseOnClose);
  CommitChangedTo(votes_.PrepareEnable(driver), true);
  CommitUnchanged(votes_.PrepareEnable(debug));
  CommitUnchanged(votes_.PrepareClose(debug));
  EXPECT_TRUE(votes_.enabled());
  EXPECT_EQ(votes_.entries().size(), 1u);
}

TEST_F(BoolVoteSetTest, ReleaseOnCloseFailureOrphans) {
  Vote debug("debug", CloseBehavior::kReleaseOnClose);
  CommitChangedTo(votes_.PrepareEnable(debug), true);

  auto plan = votes_.PrepareClose(debug);
  EXPECT_TRUE(plan.changed());
  votes_.Abandon(std::move(plan));
  EXPECT_TRUE(votes_.enabled());
  EXPECT_EQ(votes_.orphan_count(), 1u);
  ASSERT_EQ(CountLogs(LogSeverity::kWarning), 1u);
  EXPECT_THAT(logs_[0].message, HasSubstr("could not be released"));
  EXPECT_THAT(logs_[0].message, HasSubstr("debug"));

  CommitChangedTo(votes_.PrepareNodeStarted("debug"), false);
}

TEST_F(BoolVoteSetTest, Holds) {
  CommitChangedTo(votes_.PrepareAddHold("boot", true), true);
  // Re-adding a hold with the same name replaces it.
  CommitUnchanged(votes_.PrepareAddHold("boot", true));
  ASSERT_EQ(votes_.entries().size(), 1u);
  EXPECT_EQ(votes_.entries()[0].kind, EntryKind::kHold);
  EXPECT_EQ(votes_.entries()[0].key, "boot");

  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitUnchanged(votes_.PrepareEnable(vote));
  CommitUnchanged(votes_.PrepareDisable(vote));
  EXPECT_TRUE(votes_.enabled());

  // Holds are not affected by node events.
  CommitUnchanged(votes_.PrepareNodeStarted("boot"));
  EXPECT_TRUE(votes_.enabled());

  CommitUnchanged(votes_.PrepareReleaseHold("unknown"));
  CommitChangedTo(votes_.PrepareReleaseHold("boot"), false);
  EXPECT_TRUE(votes_.entries().empty());
}

TEST_F(BoolVoteSetTest, ReleaseHoldFailureKeepsHold) {
  CommitChangedTo(votes_.PrepareAddHold("boot", true), true);
  auto plan = votes_.PrepareReleaseHold("boot");
  EXPECT_TRUE(plan.changed());
  votes_.Abandon(std::move(plan));
  EXPECT_TRUE(votes_.enabled());
  EXPECT_EQ(votes_.entries().size(), 1u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, SetAndClearAreIdempotent) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareSet(vote, true), true);
  CommitUnchanged(votes_.PrepareSet(vote, true));
  EXPECT_EQ(votes_.entries()[0].count, 1u);
  CommitChangedTo(votes_.PrepareClear(vote), false);
  CommitUnchanged(votes_.PrepareClear(vote));
}

TEST_F(BoolVoteSetTest, FalseRequestsMergeToOff) {
  Vote vote("node-a", CloseBehavior::kOrphanOnClose);
  CommitUnchanged(votes_.PrepareSet(vote, false));
  EXPECT_FALSE(votes_.aggregate().has_value());
  EXPECT_EQ(votes_.entries().size(), 1u);

  // A vote that contributes nothing is dropped, not orphaned, on close.
  CommitUnchanged(votes_.PrepareClose(vote));
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.orphan_count(), 0u);
  EXPECT_EQ(votes_.orphaned_total(), 0u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, AbandonedCloseOfFalseVoteDropsIt) {
  Vote vote("debug", CloseBehavior::kReleaseOnClose);
  CommitUnchanged(votes_.PrepareSet(vote, false));
  votes_.Abandon(votes_.PrepareClose(vote));
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.orphaned_total(), 0u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, ReleaseOnCloseWithoutRequest) {
  Vote debug("debug", CloseBehavior::kReleaseOnClose);
  CommitUnchanged(votes_.PrepareClose(debug));
  EXPECT_TRUE(votes_.entries().empty());

  Vote balanced("debug", CloseBehavior::kReleaseOnClose);
  CommitChangedTo(votes_.PrepareEnable(balanced), true);
  CommitChangedTo(votes_.PrepareDisable(balanced), false);
  CommitUnchanged(votes_.PrepareClose(balanced));
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_EQ(votes_.transition_count(), 2u);
  EXPECT_TRUE(logs_.empty());
}

TEST_F(BoolVoteSetTest, StalePlanIsRejected) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  Vote b("node-b", CloseBehavior::kOrphanOnClose);
  auto plan_a = votes_.PrepareEnable(a);
  auto plan_b = votes_.PrepareEnable(b);
  votes_.Commit(std::move(plan_a));
#if ZX_DEBUG_ASSERT_IMPLEMENTED
  ASSERT_DEATH(votes_.Commit(std::move(plan_b)), "");
#else
  votes_.Commit(std::move(plan_b));
  EXPECT_EQ(votes_.entries().size(), 1u);
  EXPECT_EQ(CountLogs(LogSeverity::kError), 1u);
#endif
}

TEST_F(BoolVoteSetTest, ForeignPlanIsRejected) {
  BoolVoteSet other("other");
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  auto plan = other.PrepareEnable(a);
#if ZX_DEBUG_ASSERT_IMPLEMENTED
  ASSERT_DEATH(votes_.Commit(std::move(plan)), "");
#else
  votes_.Commit(std::move(plan));
  EXPECT_TRUE(votes_.entries().empty());
  EXPECT_FALSE(votes_.enabled());
  EXPECT_EQ(CountLogs(LogSeverity::kError), 1u);
#endif
}

TEST_F(BoolVoteSetTest, MovedFromPlanIsRejected) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(a), true);

  auto plan = votes_.PrepareDisable(a);
  auto moved = std::move(plan);
#if ZX_DEBUG_ASSERT_IMPLEMENTED
  ASSERT_DEATH(votes_.Commit(std::move(plan)), "");  // NOLINT(bugprone-use-after-move)
#else
  votes_.Commit(std::move(plan));  // NOLINT(bugprone-use-after-move)
  // The moved-from plan did not wipe the entries.
  EXPECT_EQ(votes_.entries().size(), 1u);
  EXPECT_TRUE(votes_.enabled());
  EXPECT_EQ(CountLogs(LogSeverity::kError), 1u);
#endif
  // The moved-to plan is still valid.
  CommitChangedTo(std::move(moved), false);
}

TEST_F(BoolVoteSetTest, Inspect) {
  inspect::Inspector inspector;
  votes_.AttachInspect(inspector.GetRoot().CreateChild("clk-test"));

  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  Vote b("node-b", CloseBehavior::kOrphanOnClose);
  CommitChangedTo(votes_.PrepareEnable(a), true);
  CommitUnchanged(votes_.PrepareEnable(b));
  CommitUnchanged(votes_.PrepareClose(a));
  CommitUnchanged(votes_.PrepareAddHold("boot", true));

  fpromise::result<inspect::Hierarchy> result =
      fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
  ASSERT_TRUE(result.is_ok());
  const inspect::Hierarchy* root = result.value().GetByPath({"clk-test"});
  ASSERT_NE(root, nullptr);
  EXPECT_EQ(root->node().get_property<inspect::StringPropertyValue>("resource")->value(),
            "clk-test");
  EXPECT_EQ(root->node().get_property<inspect::StringPropertyValue>("aggregate")->value(), "on");
  EXPECT_EQ(root->node().get_property<inspect::UintPropertyValue>("transition_count")->value(), 1u);
  EXPECT_EQ(root->node().get_property<inspect::UintPropertyValue>("orphan_count")->value(), 1u);
  EXPECT_EQ(root->node().get_property<inspect::UintPropertyValue>("orphaned_total")->value(), 1u);

  const inspect::Hierarchy* entries = root->GetByPath({"entries"});
  ASSERT_NE(entries, nullptr);
  ASSERT_EQ(entries->children().size(), 3u);

  auto expect_entry = [&](const std::string& index, const std::string& kind,
                          const std::string& key) {
    const inspect::Hierarchy* entry = entries->GetByPath({index});
    ASSERT_NE(entry, nullptr) << index;
    EXPECT_EQ(entry->node().get_property<inspect::StringPropertyValue>("kind")->value(), kind);
    EXPECT_EQ(entry->node().get_property<inspect::StringPropertyValue>("key")->value(), key);
    EXPECT_EQ(entry->node().get_property<inspect::StringPropertyValue>("request")->value(), "on");
    EXPECT_EQ(entry->node().get_property<inspect::UintPropertyValue>("count")->value(), 1u);
    EXPECT_GT(entry->node().get_property<inspect::IntPropertyValue>("since")->value(), 0);
  };
  expect_entry("0", "orphan", "node-a");
  expect_entry("1", "live", "node-b");
  expect_entry("2", "hold", "boot");

  CommitUnchanged(votes_.PrepareNodeStarted("node-a"));
  CommitUnchanged(votes_.PrepareReleaseHold("boot"));
  CommitChangedTo(votes_.PrepareDisable(b), false);

  result = fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
  ASSERT_TRUE(result.is_ok());
  root = result.value().GetByPath({"clk-test"});
  ASSERT_NE(root, nullptr);
  EXPECT_EQ(root->node().get_property<inspect::StringPropertyValue>("aggregate")->value(), "none");
  EXPECT_EQ(root->node().get_property<inspect::UintPropertyValue>("transition_count")->value(), 2u);
  EXPECT_EQ(root->node().get_property<inspect::UintPropertyValue>("orphan_count")->value(), 0u);
  entries = root->GetByPath({"entries"});
  ASSERT_NE(entries, nullptr);
  EXPECT_TRUE(entries->children().empty());
}

TEST(LazyInspectTest, InspectIsPopulatedLazily) {
  size_t format_calls = 0;
  inspect::Inspector inspector;

  {
    VoteSet<int> votes(
        "clk-test",
        [](const std::vector<int>& requests) -> std::optional<int> {
          int max_val = 0;
          for (int r : requests) {
            max_val = std::max(max_val, r);
          }
          return max_val > 0 ? std::optional<int>(max_val) : std::nullopt;
        },
        [&format_calls](const int& request) {
          format_calls++;
          return std::to_string(request);
        });

    votes.AttachInspect(inspector.GetRoot().CreateChild("clk-test"));

    Vote a("node-a", CloseBehavior::kOrphanOnClose);
    votes.Commit(votes.PrepareSet(a, 100));
    votes.Commit(votes.PrepareSet(a, 200));
    votes.Commit(votes.PrepareAddHold("boot", 50));
    votes.Commit(votes.PrepareReleaseHold("boot"));
    votes.Commit(votes.PrepareClear(a));
    votes.Commit(votes.PrepareSet(a, 300));

    // No formatter calls during AttachInspect or Commit.
    EXPECT_EQ(format_calls, 0u);

    // Raw VMO read (without following lazy links) has no eager properties or children on
    // "clk-test", only the inline lazy link.
    fpromise::result<inspect::Hierarchy> vmo_result =
        inspect::ReadFromVmo(inspector.DuplicateVmo());
    ASSERT_TRUE(vmo_result.is_ok());
    const inspect::Hierarchy* raw_node = vmo_result.value().GetByPath({"clk-test"});
    ASSERT_NE(raw_node, nullptr);
    EXPECT_TRUE(raw_node->node().properties().empty());
    EXPECT_TRUE(raw_node->children().empty());
    ASSERT_EQ(raw_node->node().links().size(), 1u);
    EXPECT_EQ(raw_node->node().links()[0].name(), "votes");
    EXPECT_EQ(raw_node->node().links()[0].disposition(), inspect::LinkDisposition::kInline);

    // Reading via ReadFromInspector evaluates the lazy callback on demand.
    fpromise::result<inspect::Hierarchy> result =
        fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
    ASSERT_TRUE(result.is_ok());
    EXPECT_GT(format_calls, 0u);
    const inspect::Hierarchy* node = result.value().GetByPath({"clk-test"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(node->node().get_property<inspect::StringPropertyValue>("resource")->value(),
              "clk-test");
    EXPECT_EQ(node->node().get_property<inspect::StringPropertyValue>("aggregate")->value(), "300");
    EXPECT_EQ(node->node().get_property<inspect::UintPropertyValue>("transition_count")->value(),
              4u);
    EXPECT_EQ(node->node().get_property<inspect::UintPropertyValue>("orphan_count")->value(), 0u);
    EXPECT_EQ(node->node().get_property<inspect::UintPropertyValue>("orphaned_total")->value(), 0u);
    const inspect::Hierarchy* entries = node->GetByPath({"entries"});
    ASSERT_NE(entries, nullptr);
    ASSERT_EQ(entries->children().size(), 1u);
    const inspect::Hierarchy* entry = entries->GetByPath({"0"});
    ASSERT_NE(entry, nullptr);
    EXPECT_EQ(entry->node().get_property<inspect::StringPropertyValue>("kind")->value(), "live");
    EXPECT_EQ(entry->node().get_property<inspect::StringPropertyValue>("key")->value(), "node-a");
    EXPECT_EQ(entry->node().get_property<inspect::StringPropertyValue>("request")->value(), "300");
    EXPECT_EQ(entry->node().get_property<inspect::UintPropertyValue>("count")->value(), 1u);

    // Re-attaching replaces the previous node and lazy callback cleanly.
    size_t calls_before_reattach = format_calls;
    votes.AttachInspect(inspector.GetRoot().CreateChild("clk-test-2"));
    EXPECT_EQ(format_calls, calls_before_reattach);

    result = fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
    ASSERT_TRUE(result.is_ok());
    EXPECT_EQ(result.value().GetByPath({"clk-test"}), nullptr);
    const inspect::Hierarchy* node2 = result.value().GetByPath({"clk-test-2"});
    ASSERT_NE(node2, nullptr);
    EXPECT_EQ(node2->node().get_property<inspect::StringPropertyValue>("aggregate")->value(),
              "300");
  }

  // `votes` is now destroyed; reading `inspector` must not dangle or crash.
  size_t calls_after_destroy = format_calls;
  fpromise::result<inspect::Hierarchy> after_destroy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(inspector));
  ASSERT_TRUE(after_destroy.is_ok());
  EXPECT_EQ(after_destroy.value().GetByPath({"clk-test-2"}), nullptr);
  EXPECT_EQ(format_calls, calls_after_destroy);
}

// A table-style request, as used by regulators.
struct RegulatorRequest {
  bool enable = false;
  uint32_t microvolts = 0;

  bool operator==(const RegulatorRequest&) const = default;
};

std::optional<RegulatorRequest> MergeRegulator(const std::vector<RegulatorRequest>& requests) {
  RegulatorRequest merged;
  for (const auto& request : requests) {
    merged.enable |= request.enable;
    merged.microvolts = std::max(merged.microvolts, request.microvolts);
  }
  return merged;
}

std::string FormatRegulator(const RegulatorRequest& request) {
  return std::string(request.enable ? "enabled" : "disabled") + "@" +
         std::to_string(request.microvolts) + "uV";
}

class TableVoteSetTest : public ::testing::Test {
 protected:
  TableVoteSetTest() : votes_("ldo-1", MergeRegulator, FormatRegulator) {
    votes_.SetLogSinkForTesting([this](LogSeverity severity, const std::string& message) {
      logs_.push_back({severity, message});
    });
  }

  VoteSet<RegulatorRequest> votes_;
  std::vector<LogRecord> logs_;
};

TEST_F(TableVoteSetTest, SetReplacesRequestAndMerges) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  Vote b("node-b", CloseBehavior::kOrphanOnClose);

  auto plan = votes_.PrepareSet(a, {.enable = true, .microvolts = 1'800'000});
  EXPECT_TRUE(plan.changed());
  votes_.Commit(std::move(plan));
  EXPECT_EQ(votes_.aggregate(), (RegulatorRequest{.enable = true, .microvolts = 1'800'000}));

  votes_.Commit(votes_.PrepareSet(b, {.enable = false, .microvolts = 3'300'000}));
  EXPECT_EQ(votes_.aggregate(), (RegulatorRequest{.enable = true, .microvolts = 3'300'000}));

  // Replacing a's request does not stack.
  votes_.Commit(votes_.PrepareSet(a, {.enable = false, .microvolts = 1'200'000}));
  EXPECT_EQ(votes_.aggregate(), (RegulatorRequest{.enable = false, .microvolts = 3'300'000}));
  ASSERT_EQ(votes_.entries().size(), 2u);
  EXPECT_EQ(votes_.entries()[0].count, 1u);

  plan = votes_.PrepareClear(b);
  EXPECT_EQ(plan.old_aggregate(), (RegulatorRequest{.enable = false, .microvolts = 3'300'000}));
  EXPECT_EQ(plan.new_aggregate(), (RegulatorRequest{.enable = false, .microvolts = 1'200'000}));
  votes_.Commit(std::move(plan));

  plan = votes_.PrepareClear(a);
  EXPECT_FALSE(plan.new_aggregate().has_value());
  votes_.Commit(std::move(plan));
  EXPECT_EQ(votes_.transition_count(), 5u);
}

TEST_F(TableVoteSetTest, OrphanLogFormatsRequest) {
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  votes_.Commit(votes_.PrepareSet(a, {.enable = true, .microvolts = 1'800'000}));
  auto plan = votes_.PrepareClose(a);
  EXPECT_FALSE(plan.changed());
  votes_.Commit(std::move(plan));
  ASSERT_EQ(logs_.size(), 1u);
  EXPECT_THAT(logs_[0].message, HasSubstr("ldo-1"));
  EXPECT_THAT(logs_[0].message, HasSubstr("node-a"));
  EXPECT_THAT(logs_[0].message, HasSubstr("enabled@1800000uV"));
}

TEST_F(TableVoteSetTest, DisabledRequestWithVoltageIsOrphaned) {
  // {enable = false} still contributes a voltage, so it is held on close.
  Vote a("node-a", CloseBehavior::kOrphanOnClose);
  votes_.Commit(votes_.PrepareSet(a, {.enable = false, .microvolts = 1'800'000}));
  votes_.Commit(votes_.PrepareClose(a));
  EXPECT_EQ(votes_.orphan_count(), 1u);
  EXPECT_EQ(votes_.aggregate(), (RegulatorRequest{.enable = false, .microvolts = 1'800'000}));
  EXPECT_EQ(logs_.size(), 1u);
}

TEST_F(TableVoteSetTest, ReclaimReplacesOrphanRequest) {
  Vote old_vote("node-a", CloseBehavior::kOrphanOnClose);
  votes_.Commit(votes_.PrepareSet(old_vote, {.enable = true, .microvolts = 3'300'000}));
  votes_.Commit(votes_.PrepareClose(old_vote));

  Vote new_vote("node-a", CloseBehavior::kOrphanOnClose);
  auto plan = votes_.PrepareSet(new_vote, {.enable = true, .microvolts = 1'800'000});
  EXPECT_EQ(plan.new_aggregate(), (RegulatorRequest{.enable = true, .microvolts = 1'800'000}));
  votes_.Commit(std::move(plan));
  EXPECT_EQ(votes_.orphan_count(), 0u);
}

TEST_F(TableVoteSetTest, HoldRequestReplaced) {
  votes_.Commit(votes_.PrepareAddHold("extra-client", {.enable = true, .microvolts = 0}));
  auto plan = votes_.PrepareAddHold("extra-client", {.enable = true, .microvolts = 1'000'000});
  EXPECT_TRUE(plan.changed());
  votes_.Commit(std::move(plan));
  ASSERT_EQ(votes_.entries().size(), 1u);
  EXPECT_EQ(votes_.entries()[0].request.microvolts, 1'000'000u);
}

TEST(MergeAnyTrueTest, Merge) {
  EXPECT_EQ(MergeAnyTrue({}), std::nullopt);
  EXPECT_EQ(MergeAnyTrue({false, false}), std::nullopt);
  EXPECT_EQ(MergeAnyTrue({false, true}), std::optional<bool>(true));
}

TEST(EntryKindTest, ToString) {
  EXPECT_EQ(EntryKindToString(EntryKind::kLive), "live");
  EXPECT_EQ(EntryKindToString(EntryKind::kOrphan), "orphan");
  EXPECT_EQ(EntryKindToString(EntryKind::kHold), "hold");
}

}  // namespace
}  // namespace fdf_resource_vote
