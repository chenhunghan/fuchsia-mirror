// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>

#include <bind/fuchsia/cpp/bind.h>

#include "src/devices/bin/driver_manager/tests/bind_manager_test_base.h"

class BindManagerTest : public BindManagerTestBase {};

TEST_F(BindManagerTest, BindSingleNode) {
  AddAndOrphanNode("node-a");

  // Invoke TryBindAllAvailable() in the bind manager.
  InvokeTryBindAllAvailable_EXPECT_BIND_START();
  VerifyBindOngoingWithRequests({{"node-a", 1}});

  // Invoke a driver match response from the Driver Index.
  // The node shouldn't be orphaned and bind should end.
  DriverIndexReplyWithDriver("node-a");
  VerifyOrphanedNodes({});

  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, TryBindAllAvailableWithNoNodes) {
  InvokeTryBindAllAvailable();
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, NonOverlappingRequests) {
  AddAndOrphanNode("node-a");
  AddAndOrphanNode("node-b");

  // Invoke TryBindAllAvailable() in the bind manager.
  InvokeTryBindAllAvailable_EXPECT_BIND_START();
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 1}});

  // Driver index completes the request with a match for node-a. We should have one
  // request left.
  DriverIndexReplyWithDriver("node-a");
  VerifyBindOngoingWithRequests({{"node-b", 1}});

  // Driver index completes the request with no matches for node-b.
  DriverIndexReplyWithNoMatch("node-b");
  VerifyOrphanedNodes({"node-b"});

  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, OverlappingBindRequests) {
  // Invoke bind for a new node in the bind manager.
  AddAndBindNode_EXPECT_BIND_START("node-a");
  VerifyBindOngoingWithRequests({{"node-a", 1}});

  // Add and invoke bind for two more nodes while bind is ongoing. The requests should
  // be queued.
  AddAndBindNode_EXPECT_QUEUED("node-b");
  AddAndBindNode_EXPECT_QUEUED("node-c");

  // Complete the ongoing bind process.
  DriverIndexReplyWithDriver("node-a");
  VerifyOrphanedNodes({});

  // The queued requests should be processed and kickstart a new bind process.
  VerifyBindOngoingWithRequests({{"node-b", 1}, {"node-c", 1}});
  VerifyNoQueuedBind();

  // Complete the second ongoing bind process.
  DriverIndexReplyWithDriver("node-b");
  DriverIndexReplyWithNoMatch("node-c");
  VerifyOrphanedNodes({"node-c"});

  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, BindNodeOverlapTryAllAvailable) {
  AddAndOrphanNode("node-a");
  AddAndOrphanNode("node-b");
  AddAndOrphanNode("node-c");

  InvokeTryBindAllAvailable_EXPECT_BIND_START();
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 1}, {"node-c", 1}});

  AddAndBindNode_EXPECT_QUEUED("node-d");
  AddAndBindNode_EXPECT_QUEUED("node-e");

  // Complete the ongoing bind process.
  DriverIndexReplyWithDriver("node-b");
  DriverIndexReplyWithDriver("node-a");
  DriverIndexReplyWithNoMatch("node-c");
  VerifyOrphanedNodes({"node-c"});

  // Verify that the queued requests are processed.
  VerifyBindOngoingWithRequests({{"node-d", 1}, {"node-e", 1}});
  VerifyNoQueuedBind();

  // Complete the ongoing bind. With no matches, node-d and node-e should added to the
  // orphaned nodes.
  DriverIndexReplyWithNoMatch("node-d");
  DriverIndexReplyWithNoMatch("node-e");
  VerifyOrphanedNodes({"node-c", "node-d", "node-e"});
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, OverlappingTryAllAvailable) {
  AddAndOrphanNode("node-a");
  AddAndOrphanNode("node-b");
  AddAndOrphanNode("node-c");

  // Invoke TryBindAllAvailable().
  InvokeTryBindAllAvailable_EXPECT_BIND_START();
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 1}, {"node-c", 1}});

  // Invoke TryBindAllAvailable() twice while there's an ongoing bind process. They
  // should be queued.
  InvokeTryBindAllAvailable_EXPECT_QUEUED();
  InvokeTryBindAllAvailable_EXPECT_QUEUED();

  // Match the next two nodes.
  DriverIndexReplyWithNoMatch("node-b");
  DriverIndexReplyWithNoMatch("node-a");

  // Match the final node in the ongoing bind process. This should kickstart a new
  // bind process with the queued bind requests.
  DriverIndexReplyWithDriver("node-c");
  VerifyOrphanedNodes({"node-a", "node-b"});

  // Verify that the TryBindAllAvailable() request is processed with the two orphaned
  // nodes.
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 1}});
  VerifyNoQueuedBind();

  // Complete the ongoing bind. Since the queued TryBindAllAvailable() calls are
  // consolidated, there shouldn't be a follow up bind process.
  DriverIndexReplyWithNoMatch("node-a");
  DriverIndexReplyWithNoMatch("node-b");
  VerifyOrphanedNodes({"node-a", "node-b"});
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, TryAllAvailableOverBind) {
  // Invoke bind for a new node in the bind manager.
  AddAndBindNode_EXPECT_BIND_START("node-a");
  VerifyBindOngoingWithRequests({{"node-a", 1}});

  // Invoke TryBindAllAvailable() twice. Both requests should be queued.
  InvokeTryBindAllAvailable_EXPECT_QUEUED();
  InvokeTryBindAllAvailable_EXPECT_QUEUED();

  // Complete the ongoing bind process. This should kickstart a new bind process.
  DriverIndexReplyWithNoMatch("node-a");
  VerifyBindOngoingWithRequests({{"node-a", 1}});

  // Complete the new ongoing bind process. Since the queued TryBindAllAvailable() calls
  // are consolidated, there shouldn't be a follow up bind process.
  DriverIndexReplyWithDriver("node-a");
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, OverlappingBindWithSameNode) {
  // Kickstart bind with node a.
  AddAndBindNode_EXPECT_BIND_START("node-a");
  VerifyBindOngoingWithRequests({{"node-a", 1}});

  // Queue a couple of bind requests that involves node-a.
  InvokeTryBindAllAvailable_EXPECT_QUEUED();
  InvokeBind_EXPECT_QUEUED("node-a");

  // Complete the ongoing bind process. Node-a should be bound.
  DriverIndexReplyWithDriver("node-a");

  // Since we have a pending bind request for node-a, we should make another attempt
  // to bind.
  VerifyBindOngoingWithRequests({{"node-a", 1}});
  DriverIndexReplyWithDriver("node-a");

  VerifyNoOngoingBind();
  VerifyOrphanedNodes({});
}

TEST_F(BindManagerTest, PendingBindShareSameNode) {
  AddAndOrphanNode("node-a");

  // Kickstart bind with node-b.
  AddAndBindNode_EXPECT_BIND_START("node-b");
  VerifyBindOngoingWithRequests({{"node-b", 1}});

  // Queue TryBindAllAvailable() and two Bind() for node-a.
  InvokeTryBindAllAvailable_EXPECT_QUEUED();
  InvokeBind_EXPECT_QUEUED("node-a");
  InvokeBind_EXPECT_QUEUED("node-b");
  InvokeBind_EXPECT_QUEUED("node-a");

  // Complete the ongoing bind process. It should kickstart another bind process.
  DriverIndexReplyWithNoMatch("node-b");

  // We should have two match requests for node-a since TryBindAllAvailable() should
  // exclude node-a.
  VerifyBindOngoingWithRequests({{"node-a", 2}, {"node-b", 1}});
  DriverIndexReplyWithDriver("node-a");
  DriverIndexReplyWithDriver("node-b");
  DriverIndexReplyWithDriver("node-a");

  VerifyNoOngoingBind();
  VerifyOrphanedNodes({});
}

TEST_F(BindManagerTest, AddCompositeNodeSpecThenBind) {
  AddCompositeNodeSpec("composite-a", {"node-a", "node-b"});

  // Add and bind node-a.
  AddAndBindNode("node-a");
  VerifyBindOngoingWithRequests({{"node-a", 1}});
  DriverIndexReplyWithComposite("node-a", {{"composite-a", 0}});

  // Add and bind node-b.
  AddAndBindNode("node-b");
  VerifyBindOngoingWithRequests({{"node-b", 1}});
  DriverIndexReplyWithComposite("node-b", {{"composite-a", 1}});

  VerifyCompositeNodeExists(true, "composite-a");
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, AddNodesBetweenAddingSpec) {
  AddAndOrphanNode("node-a");
  VerifyNoOngoingBind();

  // Add the spec. It should kickstart the bind process.
  AddCompositeNodeSpec("composite-a", {"node-a", "node-b"});
  VerifyBindOngoingWithRequests({{"node-a", 1}});

  DriverIndexReplyWithComposite("node-a", {{"composite-a", 0}});
  VerifyOrphanedNodes({});
  VerifyNoOngoingBind();

  // Add the remaining node for the spec.
  AddAndBindNode("node-b");
  VerifyBindOngoingWithRequests({{"node-b", 1}});

  DriverIndexReplyWithComposite("node-b", {{"composite-a", 1}});
  VerifyCompositeNodeExists(true, "composite-a");
  VerifyOrphanedNodes({});
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, AddNodesThenSpec) {
  AddAndOrphanNode("node-a");
  AddAndOrphanNode("node-b");
  VerifyNoOngoingBind();

  AddCompositeNodeSpec("composite-a", {"node-a", "node-b"});
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 1}});

  DriverIndexReplyWithComposite("node-a", {{"composite-a", 0}});
  DriverIndexReplyWithComposite("node-b", {{"composite-a", 1}});
  VerifyCompositeNodeExists(true, "composite-a");

  VerifyOrphanedNodes({});
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, TryBindForParentSpecsSkipsNonMatchingOrphans) {
  AddAndOrphanNode("node-a");
  AddAndOrphanNode("node-b");
  AddAndOrphanNode("node-c");
  VerifyNoOngoingBind();
  ASSERT_EQ(0u, CurrentBindManagerData().driver_index_request_count);

  // Build parent specs that can only match node-b's instance ID.
  uint32_t node_b_id = instance_ids()["node-b"];
  std::vector<fuchsia_driver_framework::ParentSpec2> matching_specs = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = {fdf::MakeAcceptBindRule(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID,
                                                 node_b_id)},
          .properties = {fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID, node_b_id)},
      }},
  };

  bind_manager()->TryBindForParentSpecs(matching_specs);
  RunLoopUntilIdle();

  // Only node-b should have triggered a MatchDriver request; node-a and node-c must be skipped.
  EXPECT_EQ(1u, CurrentBindManagerData().driver_index_request_count);
  VerifyBindOngoingWithRequests({{"node-a", 0}, {"node-b", 1}, {"node-c", 0}});

  // While node-b's bind is ongoing, queue a spec whose parent rules match no existing node.
  std::vector<fuchsia_driver_framework::ParentSpec2> non_matching_specs = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = {fdf::MakeAcceptBindRule(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID, 99999u)},
          .properties = {fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID, 99999u)},
      }},
  };
  bind_manager()->TryBindForParentSpecs(non_matching_specs);

  DriverIndexReplyWithDriver("node-b");
  EXPECT_EQ(0u, CurrentBindManagerData().driver_index_request_count);
  VerifyOrphanedNodes({"node-a", "node-c"});
  VerifyNoOngoingBind();
}

TEST_F(BindManagerTest, TryBindForParentSpecsEvaluatesAcceptFalseRejectAndDuplicateProtocol) {
  constexpr const char* kFlagProp = "test.optional_flag";
  constexpr const char* kRoleProp = "test.role";

  // node-missing-flag omits kFlagProp, so an Accept rule including BoolValue(false) must match it.
  AddAndOrphanNode("node-missing-flag", /*enable_multibind=*/false, nullptr,
                   {fdf::MakeProperty2(kRoleProp, std::string("target"))});

  // node-rejected has kRoleProp == "excluded", so a Reject rule for "excluded" must skip it.
  AddAndOrphanNode("node-rejected", /*enable_multibind=*/false, nullptr,
                   {fdf::MakeProperty2(kRoleProp, std::string("excluded"))});

  // node-dup-proto has two fuchsia.BIND_PROTOCOL entries; driver-index overwrites duplicate
  // BIND_PROTOCOL keys with the last entry, so matching must check the second value (20u), not 10u.
  AddAndOrphanNode("node-dup-proto", /*enable_multibind=*/false, nullptr,
                   {
                       fdf::MakeProperty2(bind_fuchsia::PROTOCOL, 10u),
                       fdf::MakeProperty2(bind_fuchsia::PROTOCOL, 20u),
                   });
  VerifyNoOngoingBind();

  // 1. Accept {false} on missing property + Reject {"excluded"} matches only node-missing-flag.
  std::vector<fuchsia_driver_framework::ParentSpec2> flag_and_reject_specs = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules =
              {
                  fdf::MakeAcceptBindRule(kFlagProp, false),
                  fdf::MakeRejectBindRule(kRoleProp, std::string("excluded")),
                  fdf::MakeAcceptBindRule(bind_fuchsia::PROTOCOL, 999u),
              },
          .properties = {fdf::MakeProperty2(kFlagProp, false)},
      }},
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules =
              {
                  fdf::MakeAcceptBindRule(kFlagProp, false),
                  fdf::MakeRejectBindRule(kRoleProp, std::string("excluded")),
                  fdf::MakeAcceptBindRule(kRoleProp, std::string("target")),
              },
          .properties = {fdf::MakeProperty2(kFlagProp, false)},
      }},
  };

  bind_manager()->TryBindForParentSpecs(flag_and_reject_specs);
  RunLoopUntilIdle();
  VerifyBindOngoingWithRequests(
      {{"node-missing-flag", 1}, {"node-rejected", 0}, {"node-dup-proto", 0}});
  DriverIndexReplyWithNoMatch("node-missing-flag");
  VerifyNoOngoingBind();

  // 2. Matching BIND_PROTOCOL == 10u (the overwritten first entry) must skip node-dup-proto,
  // whereas matching BIND_PROTOCOL == 20u (the last entry) must match it.
  std::vector<fuchsia_driver_framework::ParentSpec2> stale_proto_specs = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = {fdf::MakeAcceptBindRule(bind_fuchsia::PROTOCOL, 10u)},
          .properties = {fdf::MakeProperty2(bind_fuchsia::PROTOCOL, 10u)},
      }},
  };
  bind_manager()->TryBindForParentSpecs(stale_proto_specs);
  RunLoopUntilIdle();
  VerifyNoOngoingBind();
  EXPECT_EQ(0u, CurrentBindManagerData().driver_index_request_count);

  std::vector<fuchsia_driver_framework::ParentSpec2> latest_proto_specs = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = {fdf::MakeAcceptBindRule(bind_fuchsia::PROTOCOL, 20u)},
          .properties = {fdf::MakeProperty2(bind_fuchsia::PROTOCOL, 20u)},
      }},
  };
  bind_manager()->TryBindForParentSpecs(latest_proto_specs);
  RunLoopUntilIdle();
  VerifyBindOngoingWithRequests(
      {{"node-missing-flag", 0}, {"node-rejected", 0}, {"node-dup-proto", 1}});
  DriverIndexReplyWithDriver("node-dup-proto");
  VerifyNoOngoingBind();
  VerifyOrphanedNodes({"node-missing-flag", "node-rejected"});
}

TEST_F(BindManagerTest, TryBindForParentSpecsCoalescesDuringOngoingBindAndYieldsToBindAll) {
  AddAndOrphanNode("node-a");
  AddAndOrphanNode("node-b");
  AddAndOrphanNode("node-c");
  AddAndBindNode_EXPECT_BIND_START("node-d");

  // Queue two filtered spec requests while node-d's bind wave is ongoing; they should coalesce
  // into a single follow-up wave matching only node-a and node-c once node-d finishes.
  uint32_t node_a_id = instance_ids()["node-a"];
  uint32_t node_c_id = instance_ids()["node-c"];
  std::vector<fuchsia_driver_framework::ParentSpec2> spec_a = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = {fdf::MakeAcceptBindRule(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID,
                                                 node_a_id)},
          .properties = {fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID, node_a_id)},
      }},
  };
  std::vector<fuchsia_driver_framework::ParentSpec2> spec_c = {
      fuchsia_driver_framework::ParentSpec2{{
          .bind_rules = {fdf::MakeAcceptBindRule(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID,
                                                 node_c_id)},
          .properties = {fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_INSTANCE_ID, node_c_id)},
      }},
  };
  bind_manager()->TryBindForParentSpecs(spec_a);
  bind_manager()->TryBindForParentSpecs(spec_c);

  DriverIndexReplyWithDriver("node-d");
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 0}, {"node-c", 1}});

  // Now queue both another filtered spec and a full TryBindAllAvailable() while this wave runs:
  // TryBindAllAvailable() must supersede pending_spec_bind_filters_ without duplicate MatchDriver
  // requests.
  bind_manager()->TryBindForParentSpecs(spec_a);
  InvokeTryBindAllAvailable_EXPECT_QUEUED();

  DriverIndexReplyWithNoMatch("node-a");
  DriverIndexReplyWithDriver("node-c");

  // Only the remaining orphans (node-a and node-b) should receive one MatchDriver request each.
  VerifyBindOngoingWithRequests({{"node-a", 1}, {"node-b", 1}});
  DriverIndexReplyWithDriver("node-a");
  DriverIndexReplyWithDriver("node-b");
  VerifyNoOngoingBind();
  VerifyOrphanedNodes({});
}
