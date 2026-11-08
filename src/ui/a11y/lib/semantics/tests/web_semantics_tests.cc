// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/accessibility/semantics/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/kernel/cpp/fidl.h>
#include <fuchsia/math/cpp/fidl.h>
#include <fuchsia/process/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <fuchsia/ui/gfx/cpp/fidl.h>
#include <fuchsia/ui/input/cpp/fidl.h>
#include <fuchsia/ui/views/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fit/function.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/types.h>

#include <cmath>
#include <memory>
#include <optional>
#include <string_view>

#include <gtest/gtest.h>

#include "src/lib/fxl/strings/string_printf.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/a11y/lib/semantics/a11y_semantics_event_manager.h"
#include "src/ui/a11y/lib/view/a11y_view_semantics.h"
#include "src/ui/a11y/lib/view/tests/mocks/mock_accessibility_view.h"
#include "src/ui/a11y/lib/view/tests/mocks/mock_view_injector_factory.h"
#include "src/ui/a11y/lib/view/view_manager.h"
#include "src/ui/testing/ui_test_manager/ui_test_manager.h"
#include "src/ui/testing/ui_test_realm/ui_test_realm.h"
#include "src/ui/testing/util/web_view_client.h"

namespace accessibility_test {
namespace {

using component_testing::ChildRef;
using component_testing::Directory;
using component_testing::LocalComponentImpl;
using component_testing::ParentRef;
using component_testing::Protocol;
using component_testing::Realm;
using fuchsia::accessibility::semantics::Node;

constexpr float kDevicePixelRatio = 1.25f;

constexpr std::string_view kStaticHtml = R"(
<html>
  <head>
    <title>Title</title>
  </head>
  <body>
    <p>Paragraph</p>
    <p hidden>Hidden</p>
    <button type="button" aria-label="Button"></button>
  </body>
</html>
)";

constexpr std::string_view kDynamicHtml = R"(
<html>
  <head>
    <title>Dynamic test</title>
  </head>
  <body>
    <script>
      function incrementCounter() {
        const counter = document.querySelector('#counter');
        counter.textContent = Number.parseInt(counter.textContent, 10) + 1;
      }
    </script>
    The button has been clicked <span id="counter">0</span> times.
    <button type="button" onclick='incrementCounter()'>Increment</button>
  </body>
</html>
)";

constexpr std::string_view kScrollingHtml = R"(
<html>
  <head><title>accessibility 1</title></head>
  <body>
    <button>a button</button>
    <p>paragraph 1</p>
    <p>paragraph the second</p>
    <p>a third paragraph</p>
    <button>another button</button>
    <button>button 3</button>
    <input type="range" min="0" max="100" value="51" step="3" class="slider" id="myRange">
    <div style='height:2000px; width:2000px;'></div>
    <p>offscreen node</p>
    <button>button 4</button>
  </body>
</html>
)";

bool CompareFloat(float f0, float f1, float epsilon = 0.01f) {
  return std::abs(f0 - f1) <= epsilon;
}

// Local component that proxies SemanticsManager and SemanticTree requests to the ViewManager
// owned by the test fixture.
class SemanticsManagerProxy : public fuchsia::accessibility::semantics::SemanticsManager,
                              public LocalComponentImpl {
 public:
  SemanticsManagerProxy(fuchsia::accessibility::semantics::SemanticsManager* semantics_manager,
                        async_dispatcher_t* dispatcher)
      : dispatcher_(dispatcher), semantics_manager_(semantics_manager) {}
  ~SemanticsManagerProxy() override = default;

  // |LocalComponentImpl|
  void OnStart() override {
    FX_CHECK(outgoing()->AddPublicService(
                 fidl::InterfaceRequestHandler<fuchsia::accessibility::semantics::SemanticsManager>(
                     [this](auto request) {
                       bindings_.AddBinding(this, std::move(request), dispatcher_);
                     })) == ZX_OK);
  }

  // |fuchsia::accessibility::semantics::SemanticsManager|
  void RegisterViewForSemantics(
      fuchsia::ui::views::ViewRef view_ref,
      fidl::InterfaceHandle<fuchsia::accessibility::semantics::SemanticListener> handle,
      fidl::InterfaceRequest<fuchsia::accessibility::semantics::SemanticTree> semantic_tree_request)
      override {
    semantics_manager_->RegisterViewForSemantics(std::move(view_ref), std::move(handle),
                                                 std::move(semantic_tree_request));
  }

 private:
  async_dispatcher_t* dispatcher_ = nullptr;
  fidl::BindingSet<fuchsia::accessibility::semantics::SemanticsManager> bindings_;
  fuchsia::accessibility::semantics::SemanticsManager* semantics_manager_ = nullptr;
};

class WebSemanticsTest : public gtest::RealLoopFixture {
 public:
  static constexpr auto kSemanticsManager = "semantics_manager";
  static constexpr auto kSemanticsManagerRef = ChildRef{kSemanticsManager};

  WebSemanticsTest() : context_(sys::ComponentContext::Create()) {}
  ~WebSemanticsTest() override = default;

  void SetUp() override {
    ui_testing::UITestRealm::Config config;
    config.device_pixel_ratio = kDevicePixelRatio;
    config.use_scene_owner = true;
    config.accessibility_owner = ui_testing::UITestRealm::AccessibilityOwnerType::FAKE;
    config.ui_to_client_services = {fuchsia::ui::composition::Allocator::Name_,
                                    fuchsia::ui::composition::Flatland::Name_,
                                    fuchsia::ui::input::ImeService::Name_};
    config.passthrough_capabilities = {
        Protocol{fuchsia::kernel::VmexResource::Name_},
        Protocol{fuchsia::process::Launcher::Name_},
        Directory{
            .name = "root-ssl-certificates",
            .type = fuchsia::component::decl::DependencyType::STRONG,
        },
    };
    ui_test_manager_.emplace(config);

    BuildRealm();

    // Enable semantics before initializing the scene so that WebEngine receives the initial
    // OnSemanticsModeChanged(true) notification as soon as it registers its view.
    view_manager_->SetSemanticsEnabled(true);
    ui_test_manager_->InitializeScene();
  }

  void TearDown() override {
    web_view_client_ = nullptr;
    bool complete = false;
    ui_test_manager_->TeardownRealm(
        [&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
  }

 protected:
  a11y::ViewManager* view_manager() { return view_manager_.get(); }
  zx_koid_t view_ref_koid() const { return view_ref_koid_; }

  // Loads `html` into the WebEngine frame and waits for the view to render, the root semantic node
  // to be committed, and the root node transform to reflect the expected device pixel ratio.
  void LoadHtml(std::string_view html) {
    RunLoopUntil([this] { return web_view_client_ != nullptr && web_view_client_->is_started(); });

    web_view_client_->LoadUrl("about:blank");

    // Wait for "about:blank" navigation to complete before injecting HTML via JavaScript, so the
    // completing navigation cannot overwrite the injected document.
    RunLoopUntil([this] { return web_view_client_->is_main_document_loaded(); });

    bool js_executed = false;
    web_view_client_->ExecuteJavaScript(
        fxl::StringPrintf("document.write(`%.*s`);document.close();", static_cast<int>(html.size()),
                          html.data()),
        [&js_executed] { js_executed = true; });
    RunLoopUntil([&js_executed] { return js_executed; });

    RunLoopUntil([this]() { return ui_test_manager_->ClientViewIsRendering(); });
    auto view_ref_koid = ui_test_manager_->ClientViewRefKoid();
    ASSERT_TRUE(view_ref_koid.has_value());
    view_ref_koid_ = *view_ref_koid;

    RunLoopUntil([this] { return view_manager_->GetSemanticNode(view_ref_koid_, 0u) != nullptr; });
    WaitForScaleFactor();
  }

  // Traverses the semantic tree from the current root node to find the first node with `label`.
  const Node* FindNodeWithLabel(std::string_view label) {
    auto root = view_manager_->GetSemanticNode(view_ref_koid_, 0u);
    if (!root) {
      return nullptr;
    }
    return FindNodeWithLabel(root, label);
  }

  bool NodeExistsWithLabel(std::string_view label) { return FindNodeWithLabel(label) != nullptr; }

  void RunLoopUntilNodeExistsWithLabel(std::string_view label) {
    RunLoopUntil([this, label] { return NodeExistsWithLabel(label); });
  }

  // Calculates the point in the view's local space corresponding to the center of `node`'s
  // bounding box.
  fuchsia::math::PointF CalculateCenterOfSemanticNodeBoundingBoxCoordinate(const Node* node) {
    auto transform = view_manager_->GetNodeToRootTransform(view_ref_koid_, node->node_id());
    FX_DCHECK(transform) << "Could not compute a transform for the semantic node: "
                         << view_ref_koid_ << ":" << node->node_id();

    const auto node_bounding_box = node->location();
    const auto node_bounding_box_center_x =
        (node_bounding_box.min.x + node_bounding_box.max.x) / 2.f;
    const auto node_bounding_box_center_y =
        (node_bounding_box.min.y + node_bounding_box.max.y) / 2.f;
    const fuchsia::ui::gfx::vec3 node_bounding_box_center_local = {node_bounding_box_center_x,
                                                                   node_bounding_box_center_y, 0.f};

    const fuchsia::ui::gfx::vec3 node_bounding_box_center_root =
        transform->Apply(node_bounding_box_center_local);

    return {node_bounding_box_center_root.x, node_bounding_box_center_root.y};
  }

  // Performs a hit test at `target` in the view's local space and returns the hit node ID, if any.
  std::optional<uint32_t> HitTest(fuchsia::math::PointF target) {
    std::optional<fuchsia::accessibility::semantics::Hit> target_hit;
    view_manager_->ExecuteHitTesting(
        view_ref_koid_, target,
        [&target_hit](fuchsia::accessibility::semantics::Hit hit) { target_hit = std::move(hit); });

    RunLoopUntil([&target_hit] { return target_hit.has_value(); });
    if (!target_hit.has_value() || !target_hit->has_node_id()) {
      return std::nullopt;
    }
    return target_hit->node_id();
  }

  // Performs `action` against `node_id` and returns whether the action was handled.
  bool PerformAccessibilityAction(uint32_t node_id,
                                  fuchsia::accessibility::semantics::Action action) {
    std::optional<bool> callback_handled;
    view_manager_->PerformAccessibilityAction(
        view_ref_koid_, node_id, action,
        [&callback_handled](bool handled) { callback_handled = handled; });

    RunLoopUntil([&callback_handled] { return callback_handled.has_value(); });
    return *callback_handled;
  }

 private:
  void BuildRealm() {
    realm_ = ui_test_manager_->AddSubrealm();

    view_manager_ = std::make_unique<a11y::ViewManager>(
        std::make_unique<a11y::SemanticTreeServiceFactory>(),
        std::make_unique<a11y::A11yViewSemanticsFactory>(),
        std::make_unique<MockViewInjectorFactory>(),
        std::make_unique<a11y::A11ySemanticsEventManager>(),
        std::make_shared<MockAccessibilityView>(), context_.get());

    realm_->AddLocalChild(kSemanticsManager, [this] {
      return std::make_unique<SemanticsManagerProxy>(view_manager_.get(), dispatcher());
    });
    ui_testing::WebViewClient::AddToRealm(*realm_, dispatcher(), &web_view_client_);

    realm_->AddRoute(
        {.capabilities = {Protocol{fuchsia::accessibility::semantics::SemanticsManager::Name_}},
         .source = kSemanticsManagerRef,
         .targets = {ui_testing::WebViewClient::kChildRef}});
    realm_->AddRoute({.capabilities = {Protocol{fuchsia::ui::composition::Allocator::Name_},
                                       Protocol{fuchsia::ui::composition::Flatland::Name_},
                                       Protocol{fuchsia::ui::input::ImeService::Name_}},
                      .source = ParentRef(),
                      .targets = {ui_testing::WebViewClient::kChildRef}});

    ui_test_manager_->BuildRealm();
  }

  const Node* FindNodeWithLabel(const Node* node, std::string_view label) {
    if (!node) {
      return nullptr;
    }

    if (node->has_attributes() && node->attributes().has_label() &&
        node->attributes().label() == label) {
      return node;
    }

    if (!node->has_child_ids()) {
      return nullptr;
    }
    for (const auto& child_id : node->child_ids()) {
      const auto* child = view_manager_->GetSemanticNode(view_ref_koid_, child_id);
      FX_DCHECK(child);
      auto result = FindNodeWithLabel(child, label);
      if (result != nullptr) {
        return result;
      }
    }

    return nullptr;
  }

  // Waits for the root semantic node's transform to include a scale of 1 / kDevicePixelRatio.
  //
  // This scale factor is required for hit testing and scrolling, but may not be present in the
  // first committed version of the semantic tree.
  void WaitForScaleFactor() {
    RunLoopUntil([this] {
      auto node = view_manager_->GetSemanticNode(view_ref_koid_, 0u);
      return node && node->has_node_to_container_transform() &&
             CompareFloat(node->node_to_container_transform().matrix[0], 1 / kDevicePixelRatio);
    });
  }

  zx_koid_t view_ref_koid_ = ZX_KOID_INVALID;
  std::unique_ptr<sys::ComponentContext> context_;
  std::optional<Realm> realm_;
  std::unique_ptr<a11y::ViewManager> view_manager_;
  std::optional<ui_testing::UITestManager> ui_test_manager_;
  ui_testing::WebViewClient* web_view_client_ = nullptr;
};

TEST_F(WebSemanticsTest, StaticSemantics) {
  /* The semantic tree for kStaticHtml:
   *
   * ID: 0 Label:Title Role: UNKNOWN
   *     ID: 2 Label:no label Role: UNKNOWN
   *         ID: 3 Label:no label Role: UNKNOWN
   *             ID: 4 Label:no label Role: UNKNOWN
   *                 ID: 6 Label:Paragraph Role: STATIC_TEXT
   *                     ID: 8 Label:Paragraph Role: UNKNOWN
   *             ID: 5 Label:Button Role: BUTTON
   */
  LoadHtml(kStaticHtml);

  RunLoopUntilNodeExistsWithLabel("Title");
  RunLoopUntilNodeExistsWithLabel("Paragraph");
  EXPECT_FALSE(NodeExistsWithLabel("Hidden"));
}

TEST_F(WebSemanticsTest, HitTesting) {
  LoadHtml(kStaticHtml);

  // Hit test the plain text.
  RunLoopUntilNodeExistsWithLabel("Paragraph");
  auto node = FindNodeWithLabel("Paragraph");
  ASSERT_TRUE(node);
  auto hit_node = HitTest(CalculateCenterOfSemanticNodeBoundingBoxCoordinate(node));
  ASSERT_TRUE(hit_node.has_value());
  ASSERT_EQ(*hit_node, node->node_id());

  // Hit test the button.
  RunLoopUntilNodeExistsWithLabel("Button");
  node = FindNodeWithLabel("Button");
  ASSERT_TRUE(node);
  hit_node = HitTest(CalculateCenterOfSemanticNodeBoundingBoxCoordinate(node));
  ASSERT_TRUE(hit_node.has_value());
  ASSERT_EQ(*hit_node, node->node_id());
}

TEST_F(WebSemanticsTest, PerformAction) {
  LoadHtml(kDynamicHtml);

  // Find the node with the counter to make sure it still reads 0
  RunLoopUntilNodeExistsWithLabel("0");
  // There shouldn't be a node labeled 1 yet
  EXPECT_FALSE(NodeExistsWithLabel("1"));

  // Trigger the button's default action
  auto node = FindNodeWithLabel("Increment");
  ASSERT_TRUE(node);
  EXPECT_TRUE(node->has_role() && node->role() == fuchsia::accessibility::semantics::Role::BUTTON);
  bool callback_handled = PerformAccessibilityAction(
      node->node_id(), fuchsia::accessibility::semantics::Action::DEFAULT);
  EXPECT_TRUE(callback_handled);

  RunLoopUntilNodeExistsWithLabel("1");
}

TEST_F(WebSemanticsTest, ScrollToMakeVisible) {
  LoadHtml(kScrollingHtml);

  // The offscreen node should be off-screen.
  RunLoopUntilNodeExistsWithLabel("offscreen node");
  auto node = FindNodeWithLabel("offscreen node");
  ASSERT_TRUE(node);

  bool callback_handled = PerformAccessibilityAction(
      node->node_id(), fuchsia::accessibility::semantics::Action::SHOW_ON_SCREEN);
  EXPECT_TRUE(callback_handled);

  // Verify that the root container was scrolled to make the offscreen node
  // visible.
  RunLoopUntil([this] {
    auto root = view_manager()->GetSemanticNode(view_ref_koid(), 0u);
    return root && root->has_states() && root->states().has_viewport_offset() &&
           root->states().viewport_offset().y != 0;
  });
}

}  // namespace
}  // namespace accessibility_test
