// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/server.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/common/test_with_loop.h"
#include "src/developer/debug/zxdb/console/mock_console.h"

namespace zxdb {

namespace {

struct SocketAddress {
  sockaddr_storage storage{};
  socklen_t len = 0;
};

template <typename SockAddr>
SocketAddress ToSocketAddress(const SockAddr& sa) {
  static_assert(sizeof(SockAddr) <= sizeof(sockaddr_storage));
  SocketAddress addr;
  memcpy(&addr.storage, &sa, sizeof(sa));
  addr.len = sizeof(sa);
  return addr;
}

SocketAddress MakeIPv4Address(uint32_t host_order_ip, uint16_t port) {
  sockaddr_in sin{};
  sin.sin_family = AF_INET;
  sin.sin_addr.s_addr = htonl(host_order_ip);
  sin.sin_port = htons(port);
  return ToSocketAddress(sin);
}

SocketAddress MakeIPv6Address(const in6_addr& ip, uint16_t port) {
  sockaddr_in6 sin6{};
  sin6.sin6_family = AF_INET6;
  sin6.sin6_addr = ip;
  sin6.sin6_port = htons(port);
  return ToSocketAddress(sin6);
}

std::string FormatSocketAddress(const SocketAddress& addr) {
  char buf[INET6_ADDRSTRLEN] = {};
  if (addr.storage.ss_family == AF_INET) {
    sockaddr_in sin{};
    memcpy(&sin, &addr.storage, sizeof(sin));
    inet_ntop(AF_INET, &sin.sin_addr, buf, sizeof(buf));
    return std::string(buf) + ":" + std::to_string(ntohs(sin.sin_port));
  }
  if (addr.storage.ss_family == AF_INET6) {
    sockaddr_in6 sin6{};
    memcpy(&sin6, &addr.storage, sizeof(sin6));
    inet_ntop(AF_INET6, &sin6.sin6_addr, buf, sizeof(buf));
    std::string scope = sin6.sin6_scope_id ? "%" + std::to_string(sin6.sin6_scope_id) : "";
    return "[" + std::string(buf) + scope + "]:" + std::to_string(ntohs(sin6.sin6_port));
  }
  return "<unknown>";
}

bool IsIPv6LoopbackAvailable() {
  fbl::unique_fd fd(socket(AF_INET6, SOCK_STREAM, IPPROTO_TCP));
  if (!fd.is_valid()) {
    return false;
  }
  SocketAddress loopback = MakeIPv6Address(in6addr_loopback, 0);
  return bind(fd.get(), reinterpret_cast<const sockaddr*>(&loopback.storage), loopback.len) == 0;
}

std::vector<SocketAddress> CollectNonLoopbackAddresses(uint16_t port) {
  std::vector<SocketAddress> candidates;
  ifaddrs* ifap = nullptr;
  if (getifaddrs(&ifap) == 0) {
    for (ifaddrs* ifa = ifap; ifa != nullptr; ifa = ifa->ifa_next) {
      if (!ifa->ifa_addr || !(ifa->ifa_flags & IFF_UP) || (ifa->ifa_flags & IFF_LOOPBACK)) {
        continue;
      }
      if (ifa->ifa_addr->sa_family == AF_INET) {
        sockaddr_in sin{};
        memcpy(&sin, ifa->ifa_addr, sizeof(sin));
        if (ntohl(sin.sin_addr.s_addr) == INADDR_LOOPBACK ||
            ntohl(sin.sin_addr.s_addr) == INADDR_ANY) {
          continue;
        }
        sin.sin_port = htons(port);
        candidates.push_back(ToSocketAddress(sin));
      } else if (ifa->ifa_addr->sa_family == AF_INET6) {
        sockaddr_in6 sin6{};
        memcpy(&sin6, ifa->ifa_addr, sizeof(sin6));
        if (IN6_IS_ADDR_LOOPBACK(&sin6.sin6_addr) || IN6_IS_ADDR_UNSPECIFIED(&sin6.sin6_addr)) {
          continue;
        }
        sin6.sin6_port = htons(port);
        if (IN6_IS_ADDR_LINKLOCAL(&sin6.sin6_addr) && sin6.sin6_scope_id == 0) {
          sin6.sin6_scope_id = if_nametoindex(ifa->ifa_name);
        }
        candidates.push_back(ToSocketAddress(sin6));
      }
    }
    freeifaddrs(ifap);
  }
#ifdef __linux__
  // Linux routes 127.0.0.0/8 on lo, so 127.0.0.2 reaches wildcard listeners even in isolated
  // network namespaces without external interfaces.
  candidates.push_back(MakeIPv4Address(0x7f000002, port));
#endif
  return candidates;
}

bool CanConnectTcp(const SocketAddress& addr) {
  fbl::unique_fd fd(socket(addr.storage.ss_family, SOCK_STREAM, IPPROTO_TCP));
  if (!fd.is_valid()) {
    return false;
  }
  if (fcntl(fd.get(), F_SETFL, O_NONBLOCK) < 0) {
    return false;
  }
  if (connect(fd.get(), reinterpret_cast<const sockaddr*>(&addr.storage), addr.len) == 0) {
    return true;
  }
  if (errno != EINPROGRESS) {
    return false;
  }
  pollfd pfd{};
  pfd.fd = fd.get();
  pfd.events = POLLOUT;
  if (poll(&pfd, 1, 300) <= 0) {
    return false;
  }
  int so_error = 0;
  socklen_t optlen = sizeof(so_error);
  if (getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &so_error, &optlen) < 0) {
    return false;
  }
  return so_error == 0;
}

// Socket client to test the server
class TestClient {
 public:
  bool Connect(uint16_t port) {
    if (IsIPv6LoopbackAvailable()) {
      return Connect(MakeIPv6Address(in6addr_loopback, port));
    }
    return Connect(MakeIPv4Address(INADDR_LOOPBACK, port));
  }

  bool Connect(const SocketAddress& addr) {
    socket_.reset(socket(addr.storage.ss_family, SOCK_STREAM, IPPROTO_TCP));
    if (!socket_.is_valid()) {
      FX_LOGS(ERROR) << "Could not create socket.";
      return false;
    }
    if (connect(socket_.get(), reinterpret_cast<const sockaddr*>(&addr.storage), addr.len)) {
      FX_LOGS(ERROR) << "Could not connect to " << FormatSocketAddress(addr);
      return false;
    }
    return true;
  }

  void Disconnect() { socket_.reset(); }

 private:
  fbl::unique_fd socket_;
};

constexpr uint16_t kServerPort = 15678;

class DebugAdapterServerTest : public TestWithLoop, public DebugAdapterServerObserver {
 public:
  DebugAdapterServerTest() : console_(&session_), server_(&console_, kServerPort) {
    console_.Init();
    server_.AddObserver(this);
  }

  DebugAdapterServer& server() { return server_; }
  MockConsole& console() { return console_; }

  // DebugAdapterServerObserver methods. Quit loop to continue with the tests.
  void ClientConnected() override { debug::MessageLoop::Current()->QuitNow(); }
  void ClientDisconnected() override { debug::MessageLoop::Current()->QuitNow(); }

 private:
  Session session_;
  MockConsole console_;
  DebugAdapterServer server_;
};

TEST_F(DebugAdapterServerTest, InitTest) { server().Init(); }

TEST_F(DebugAdapterServerTest, ConnectionTest) {
  auto err = server().Init();
  ASSERT_FALSE(err.has_error());
  TestClient client;
  ASSERT_TRUE(client.Connect(kServerPort));
  // Loop is quit once the observer is notified of the connection
  debug::MessageLoop::Current()->Run();
  EXPECT_TRUE(server().IsConnected());
}

TEST_F(DebugAdapterServerTest, ConnectDisconnectTest) {
  auto err = server().Init();
  ASSERT_FALSE(err.has_error());
  TestClient client;
  ASSERT_TRUE(client.Connect(kServerPort));

  // Loop is quit once the observer is notified of the connection
  debug::MessageLoop::Current()->Run();
  ASSERT_TRUE(server().IsConnected());
  client.Disconnect();

  // Loop is quit once the observer is notified of the disconnection
  debug::MessageLoop::Current()->Run();
  EXPECT_FALSE(server().IsConnected());
}

TEST_F(DebugAdapterServerTest, AcceptsIPv4AndIPv6Loopback) {
  Err err = server().Init();
  ASSERT_FALSE(err.has_error()) << err.msg();

  TestClient ipv4_client;
  ASSERT_TRUE(ipv4_client.Connect(MakeIPv4Address(INADDR_LOOPBACK, kServerPort)));
  debug::MessageLoop::Current()->Run();
  ASSERT_TRUE(server().IsConnected());

  ipv4_client.Disconnect();
  debug::MessageLoop::Current()->Run();
  ASSERT_FALSE(server().IsConnected());

  if (IsIPv6LoopbackAvailable()) {
    TestClient ipv6_client;
    ASSERT_TRUE(ipv6_client.Connect(MakeIPv6Address(in6addr_loopback, kServerPort)));
    debug::MessageLoop::Current()->Run();
    ASSERT_TRUE(server().IsConnected());

    ipv6_client.Disconnect();
    debug::MessageLoop::Current()->Run();
    EXPECT_FALSE(server().IsConnected());
  }
}

TEST_F(DebugAdapterServerTest, RejectsNonLoopbackConnections) {
  Err err = server().Init();
  ASSERT_FALSE(err.has_error()) << err.msg();

  std::vector<SocketAddress> candidates = CollectNonLoopbackAddresses(kServerPort);
#ifdef __linux__
  ASSERT_FALSE(candidates.empty());
#else
  if (candidates.empty()) {
    GTEST_SKIP() << "No non-loopback host addresses available to probe.";
  }
#endif

  // Probe without running the message loop so unexpected connections fail immediately rather than
  // blocking teardown.
  for (const SocketAddress& candidate : candidates) {
    EXPECT_FALSE(CanConnectTcp(candidate))
        << "Unexpectedly connected to non-loopback address " << FormatSocketAddress(candidate);
  }
}

TEST_F(DebugAdapterServerTest, FailsInitWhenLoopbackPortInUse) {
  fbl::unique_fd blocker(socket(AF_INET, SOCK_STREAM, IPPROTO_TCP));
  ASSERT_TRUE(blocker.is_valid());

  SocketAddress bind_addr = MakeIPv4Address(INADDR_LOOPBACK, 0);
  ASSERT_EQ(
      0, bind(blocker.get(), reinterpret_cast<const sockaddr*>(&bind_addr.storage), bind_addr.len));
  ASSERT_EQ(0, listen(blocker.get(), 1));

  sockaddr_in assigned{};
  socklen_t assigned_len = sizeof(assigned);
  ASSERT_EQ(0, getsockname(blocker.get(), reinterpret_cast<sockaddr*>(&assigned), &assigned_len));
  const uint16_t occupied_port = ntohs(assigned.sin_port);
  ASSERT_NE(0, occupied_port);

  DebugAdapterServer local_server(&console(), occupied_port);
  Err err = local_server.Init();
  EXPECT_TRUE(err.has_error());
  EXPECT_FALSE(local_server.IsConnected());

  // Failed initialization must not leave an IPv6 loopback listener open on the port.
  EXPECT_FALSE(CanConnectTcp(MakeIPv6Address(in6addr_loopback, occupied_port)));
}

}  // namespace

}  // namespace zxdb
