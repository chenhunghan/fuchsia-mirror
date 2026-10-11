// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/network/mdns/util/mdns_impl.h"

#include <fuchsia/net/mdns/cpp/fidl.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/sys/cpp/testing/component_context_provider.h>

#include "src/connectivity/network/mdns/util/commands.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace mdns {
namespace test {

class MdnsImplTests : public gtest::RealLoopFixture,
                      public fuchsia::net::mdns::ServiceInstancePublisher,
                      public fuchsia::net::mdns::ProxyHostPublisher {
 public:
  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    context_provider_.service_directory_provider()->AddService(
        service_instance_publisher_bindings_.GetHandler(this));
    context_provider_.service_directory_provider()->AddService(
        proxy_host_publisher_bindings_.GetHandler(this));
  }

  // fuchsia::net::mdns::ServiceInstancePublisher implementation.
  void PublishServiceInstance(
      std::string service, std::string instance,
      fuchsia::net::mdns::ServiceInstancePublicationOptions options,
      fidl::InterfaceHandle<fuchsia::net::mdns::ServiceInstancePublicationResponder>
          publication_responder,
      PublishServiceInstanceCallback callback) override {
    ++publish_service_instance_count_;
    publication_responder_ = publication_responder.Bind();
    callback(
        fuchsia::net::mdns::ServiceInstancePublisher_PublishServiceInstance_Result::WithResponse(
            {}));
  }

  // fuchsia::net::mdns::ProxyHostPublisher implementation.
  void PublishProxyHost(std::string host, std::vector<fuchsia::net::IpAddress> addresses,
                        fuchsia::net::mdns::ProxyHostPublicationOptions options,
                        fidl::InterfaceRequest<fuchsia::net::mdns::ServiceInstancePublisher>
                            service_instance_publisher,
                        PublishProxyHostCallback callback) override {
    ++publish_proxy_host_count_;
    service_instance_publisher_bindings_.AddBinding(this, std::move(service_instance_publisher));
    callback(fuchsia::net::mdns::ProxyHostPublisher_PublishProxyHost_Result::WithResponse({}));
  }

  sys::ComponentContext* context() { return context_provider_.context(); }

  size_t publish_service_instance_count() {
    auto result = publish_service_instance_count_;
    publish_service_instance_count_ = 0;
    return result;
  }

  size_t publish_proxy_host_count() {
    auto result = publish_proxy_host_count_;
    publish_proxy_host_count_ = 0;
    return result;
  }

  fuchsia::net::mdns::ServiceInstancePublicationResponderPtr& publication_responder() {
    return publication_responder_;
  }

 private:
  sys::testing::ComponentContextProvider context_provider_;
  fidl::BindingSet<fuchsia::net::mdns::ServiceInstancePublisher>
      service_instance_publisher_bindings_;
  fidl::BindingSet<fuchsia::net::mdns::ProxyHostPublisher> proxy_host_publisher_bindings_;
  fuchsia::net::mdns::ServiceInstancePublicationResponderPtr publication_responder_;
  size_t publish_service_instance_count_ = 0;
  size_t publish_proxy_host_count_ = 0;
};

// Tests that disconnecting the responder channel for a published service instance cleans up
// without use-after-free (regression test for b/521973679).
TEST_F(MdnsImplTests, PublishInstanceResponderDisconnect) {
  CommandParser parser("publish myinstance._myservice._tcp. 1234 \"\"");
  MdnsImpl under_test(context(), parser.Parse(), dispatcher(), []() {});

  RunLoopUntilIdle();
  EXPECT_EQ(1u, publish_service_instance_count());
  EXPECT_TRUE(publication_responder().is_bound());

  // Disconnect the responder channel, triggering the error handler in MdnsImpl.
  publication_responder().Unbind();
  RunLoopUntilIdle();

  // Publishing the same instance again should succeed because the disconnected responder was
  // erased.
  CommandParser republish_parser("publish myinstance._myservice._tcp. 1234 \"\"");
  under_test.ExecuteCommand(republish_parser.Parse());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, publish_service_instance_count());
  EXPECT_TRUE(publication_responder().is_bound());
}

// Tests that disconnecting the responder channel for a service instance published on a proxy host
// cleans up without use-after-free (regression test for b/521973679).
TEST_F(MdnsImplTests, PublishInstanceOnProxyHostResponderDisconnect) {
  CommandParser host_parser("publish myproxy.local. 192.168.1.1");
  MdnsImpl under_test(context(), host_parser.Parse(), dispatcher(), []() {});

  RunLoopUntilIdle();
  EXPECT_EQ(1u, publish_proxy_host_count());

  CommandParser instance_parser(
      "publish myinstance._myservice._tcp. 1234 \"\" --proxy-host=myproxy.local.");
  under_test.ExecuteCommand(instance_parser.Parse());

  RunLoopUntilIdle();
  EXPECT_EQ(1u, publish_service_instance_count());
  EXPECT_TRUE(publication_responder().is_bound());

  // Disconnect the responder channel, triggering the error handler in MdnsImpl.
  publication_responder().Unbind();
  RunLoopUntilIdle();

  // Publishing the same instance on the proxy host again should succeed because the disconnected
  // responder was removed.
  CommandParser republish_parser(
      "publish myinstance._myservice._tcp. 1234 \"\" --proxy-host=myproxy.local.");
  under_test.ExecuteCommand(republish_parser.Parse());
  RunLoopUntilIdle();
  EXPECT_EQ(1u, publish_service_instance_count());
  EXPECT_TRUE(publication_responder().is_bound());
}

}  // namespace test
}  // namespace mdns
