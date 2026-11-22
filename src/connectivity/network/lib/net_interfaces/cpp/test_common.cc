// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "test_common.h"

#include <fuchsia/net/cpp/fidl.h>
#include <fuchsia/net/interfaces/cpp/fidl.h>

namespace net::interfaces::test {

fuchsia::net::Ipv4Address ToFIDL(const std::array<uint8_t, 4>& bytes) {
  fuchsia::net::Ipv4Address addr;
  std::copy(bytes.cbegin(), bytes.cend(), addr.addr.begin());
  return addr;
}

fuchsia::net::Ipv6Address ToFIDL(const std::array<uint8_t, 16>& bytes) {
  fuchsia::net::Ipv6Address addr;
  std::copy(bytes.cbegin(), bytes.cend(), addr.addr.begin());
  return addr;
}

void InitAddress(fuchsia::net::interfaces::Address& addr, const std::array<uint8_t, 4>& bytes) {
  fuchsia::net::Subnet subnet{
      .prefix_len = kIPv4PrefixLength,
  };
  subnet.addr.set_ipv4(ToFIDL(bytes));
  addr.set_addr(std::move(subnet));
}

void InitAddress(fuchsia::net::interfaces::Address& addr, const std::array<uint8_t, 16>& bytes) {
  fuchsia::net::Subnet subnet{
      .prefix_len = kIPv6PrefixLength,
  };
  subnet.addr.set_ipv6(ToFIDL(bytes));
  addr.set_addr(std::move(subnet));
}

void AppendAddresses(std::vector<fuchsia::net::interfaces::Address>& addresses) {}

void SetMutableProperties(fuchsia::net::interfaces::Properties& properties, bool online,
                          bool has_default_ipv4_route, bool has_default_ipv6_route,
                          std::vector<fuchsia::net::interfaces::Address> addresses) {
  properties.set_online(online);
  properties.set_has_default_ipv4_route(has_default_ipv4_route);
  properties.set_has_default_ipv6_route(has_default_ipv6_route);
  properties.set_addresses(std::move(addresses));
}

}  // namespace net::interfaces::test
