// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <fidl/fuchsia.net/cpp/wire.h>
#include <fidl/fuchsia.posix.socket.packet/cpp/wire_test_base.h>
#include <fidl/fuchsia.posix.socket.raw/cpp/wire_test_base.h>
#include <fidl/fuchsia.posix.socket/cpp/wire_test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/socket.h>
#include <lib/zxio/cpp/create_with_type.h>
#include <lib/zxio/zxio.h>
#include <zircon/types.h>

#include <zxtest/zxtest.h>

#include "sdk/lib/zxio/private.h"
#include "sdk/lib/zxio/socket_address.h"
#include "sdk/lib/zxio/tests/test_socket_server.h"

namespace fnet = fuchsia_net;
namespace fposix = fuchsia_posix;
namespace fsocket = fuchsia_posix_socket;
namespace fsocket_packet = fuchsia_posix_socket_packet;
namespace fsocket_raw = fuchsia_posix_socket_raw;

namespace {

class SynchronousDatagramSocketTest : public zxtest::Test {
 public:
  void SetUp() final {
    ASSERT_OK(zx::eventpair::create(0u, &event0_, &event1_));

    zx::result node_server = fidl::CreateEndpoints(&client_end_);
    ASSERT_OK(node_server.status_value());

    fidl::BindServer(control_loop_.dispatcher(), std::move(*node_server), &server_);
    control_loop_.StartThread("control");
  }

  void Init() {
    ASSERT_OK(zxio_synchronous_datagram_socket_init(&storage_, TakeEvent(), TakeClientEnd()));
    zxio_ = &storage_.io;
  }

  void TearDown() final {
    if (zxio_) {
      ASSERT_OK(zxio_close(zxio_));
      zxio_destroy(zxio_);
    }
    control_loop_.Shutdown();
  }

  zx::eventpair TakeEvent() { return std::move(event0_); }
  fidl::ClientEnd<fsocket::SynchronousDatagramSocket> TakeClientEnd() {
    return std::move(client_end_);
  }
  zxio_storage_t* storage() { return &storage_; }
  zxio_t* zxio() { return zxio_; }

 private:
  zxio_storage_t storage_;
  zxio_t* zxio_{nullptr};
  zx::eventpair event0_, event1_;
  fidl::ClientEnd<fsocket::SynchronousDatagramSocket> client_end_;
  zxio_tests::SynchronousDatagramSocketServer server_{{}};
  async::Loop control_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
};

TEST_F(SynchronousDatagramSocketTest, Basic) { Init(); }

TEST_F(SynchronousDatagramSocketTest, Release) {
  Init();

  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_release(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);

  EXPECT_OK(zx_handle_close(handle));
}

TEST_F(SynchronousDatagramSocketTest, Borrow) {
  Init();

  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_borrow(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);
}

TEST_F(SynchronousDatagramSocketTest, CreateWithType) {
  ASSERT_OK(zxio_create_with_type(storage(), ZXIO_OBJECT_TYPE_SYNCHRONOUS_DATAGRAM_SOCKET,
                                  TakeEvent().release(), TakeClientEnd().TakeChannel().release()));
  ASSERT_OK(zxio_close(&storage()->io));
  zxio_destroy(&storage()->io);
}

TEST_F(SynchronousDatagramSocketTest, Attr) {
  Init();

  zxio_node_attributes_t attr = {.has = {.object_type = true}};
  ASSERT_OK(zxio_attr_get(zxio(), &attr));
  EXPECT_EQ(ZXIO_OBJECT_TYPE_SYNCHRONOUS_DATAGRAM_SOCKET, attr.object_type);
}

class StreamSocketTest : public zxtest::Test {
 public:
  void SetUp() final {
    ASSERT_OK(zx::socket::create(ZX_SOCKET_STREAM, &socket_, &peer_));
    ASSERT_OK(socket_.get_info(ZX_INFO_SOCKET, &info_, sizeof(info_), nullptr, nullptr));

    zx::result server_end = fidl::CreateEndpoints(&client_end_);
    ASSERT_OK(server_end.status_value());

    fidl::BindServer(control_loop_.dispatcher(), std::move(*server_end), &server_);
    control_loop_.StartThread("control");
  }

  void Init() {
    ASSERT_OK(zxio_stream_socket_init(&storage_, TakeSocket(), info(), /*is_connected=*/false,
                                      TakeClientEnd()));
    zxio_ = &storage_.io;
  }

  void TearDown() final {
    if (zxio_) {
      ASSERT_OK(zxio_close(zxio_));
      zxio_destroy(zxio_);
    }
    control_loop_.Shutdown();
  }

  zx_info_socket_t& info() { return info_; }
  zx::socket TakeSocket() { return std::move(socket_); }
  fidl::ClientEnd<fsocket::StreamSocket> TakeClientEnd() { return std::move(client_end_); }
  zxio_storage_t* storage() { return &storage_; }
  zxio_t* zxio() { return zxio_; }

 private:
  zxio_storage_t storage_;
  zxio_t* zxio_{nullptr};
  zx_info_socket_t info_;
  zx::socket socket_, peer_;
  fidl::ClientEnd<fsocket::StreamSocket> client_end_;
  zxio_tests::StreamSocketServer server_{{}};
  async::Loop control_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
};

TEST_F(StreamSocketTest, Basic) { Init(); }

TEST_F(StreamSocketTest, Release) {
  Init();

  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_release(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);

  EXPECT_OK(zx_handle_close(handle));
}

TEST_F(StreamSocketTest, Borrow) {
  Init();

  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_borrow(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);
}

TEST_F(StreamSocketTest, CreateWithType) {
  ASSERT_OK(zxio_create_with_type(storage(), ZXIO_OBJECT_TYPE_STREAM_SOCKET, TakeSocket().release(),
                                  &info(), /*is_connected=*/false,
                                  TakeClientEnd().TakeChannel().release()));
  ASSERT_OK(zxio_close(&storage()->io));
  zxio_destroy(&storage()->io);
}

TEST_F(StreamSocketTest, Attr) {
  Init();

  zxio_node_attributes_t attr = {.has = {.object_type = true}};
  ASSERT_OK(zxio_attr_get(zxio(), &attr));
  EXPECT_EQ(ZXIO_OBJECT_TYPE_STREAM_SOCKET, attr.object_type);
}

class RawSocketTest : public zxtest::Test {
 public:
  void SetUp() final {
    ASSERT_OK(zx::eventpair::create(0u, &event_client_, &event_server_));

    zx::result server_end = fidl::CreateEndpoints(&client_end_);
    ASSERT_OK(server_end.status_value());

    fidl::BindServer(control_loop_.dispatcher(), std::move(*server_end), &server_);
    control_loop_.StartThread("control");
  }

  void Init() {
    ASSERT_OK(zxio_raw_socket_init(&storage_, TakeEventClient(), TakeClientEnd()));
    zxio_ = &storage_.io;
  }

  void TearDown() final {
    if (zxio_) {
      ASSERT_OK(zxio_close(zxio_));
      zxio_destroy(zxio_);
    }
    control_loop_.Shutdown();
  }

  zx::eventpair TakeEventClient() { return std::move(event_client_); }
  fidl::ClientEnd<fsocket_raw::Socket> TakeClientEnd() { return std::move(client_end_); }
  zxio_storage_t* storage() { return &storage_; }
  zxio_t* zxio() { return zxio_; }

 private:
  zxio_storage_t storage_;
  zxio_t* zxio_{nullptr};
  zx::eventpair event_client_, event_server_;
  fidl::ClientEnd<fsocket_raw::Socket> client_end_;
  zxio_tests::RawSocketServer server_{{}};
  async::Loop control_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
};

TEST_F(RawSocketTest, Basic) { Init(); }

TEST_F(RawSocketTest, Release) {
  Init();
  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_release(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);

  EXPECT_OK(zx_handle_close(handle));
}

TEST_F(RawSocketTest, Borrow) {
  Init();
  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_borrow(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);
}

TEST_F(RawSocketTest, CreateWithType) {
  ASSERT_OK(zxio_create_with_type(storage(), ZXIO_OBJECT_TYPE_RAW_SOCKET,
                                  TakeEventClient().release(),
                                  TakeClientEnd().TakeChannel().release()));
  ASSERT_OK(zxio_close(&storage()->io));
  zxio_destroy(&storage()->io);
}

TEST_F(RawSocketTest, Attr) {
  Init();

  zxio_node_attributes_t attr = {.has = {.object_type = true}};
  ASSERT_OK(zxio_attr_get(zxio(), &attr));
  EXPECT_EQ(ZXIO_OBJECT_TYPE_RAW_SOCKET, attr.object_type);
}

class PacketSocketTest : public zxtest::Test {
 public:
  void SetUp() final {
    ASSERT_OK(zx::eventpair::create(0u, &event_client_, &event_server_));

    zx::result server_end = fidl::CreateEndpoints(&client_end_);
    ASSERT_OK(server_end.status_value());

    fidl::BindServer(control_loop_.dispatcher(), std::move(*server_end), &server_);
    control_loop_.StartThread("control");
  }

  void Init() {
    ASSERT_OK(zxio_packet_socket_init(&storage_, TakeEventClient(), TakeClientEnd()));
    zxio_ = &storage_.io;
  }

  void TearDown() final {
    if (zxio_) {
      ASSERT_OK(zxio_close(zxio_));
      zxio_destroy(zxio_);
    }
    control_loop_.Shutdown();
  }

  zx::eventpair TakeEventClient() { return std::move(event_client_); }
  fidl::ClientEnd<fsocket_packet::Socket> TakeClientEnd() { return std::move(client_end_); }
  zxio_storage_t* storage() { return &storage_; }
  zxio_t* zxio() { return zxio_; }

 private:
  zxio_storage_t storage_;
  zxio_t* zxio_{nullptr};
  zx::eventpair event_client_, event_server_;
  fidl::ClientEnd<fsocket_packet::Socket> client_end_;
  zxio_tests::PacketSocketServer server_{{}};
  async::Loop control_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
};

TEST_F(PacketSocketTest, Basic) { Init(); }

TEST_F(PacketSocketTest, Release) {
  Init();
  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_release(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);

  EXPECT_OK(zx_handle_close(handle));
}

TEST_F(PacketSocketTest, Borrow) {
  Init();
  zx_handle_t handle = ZX_HANDLE_INVALID;
  EXPECT_OK(zxio_borrow(zxio(), &handle));
  EXPECT_NE(handle, ZX_HANDLE_INVALID);
}

TEST_F(PacketSocketTest, CreateWithType) {
  ASSERT_OK(zxio_create_with_type(storage(), ZXIO_OBJECT_TYPE_PACKET_SOCKET,
                                  TakeEventClient().release(),
                                  TakeClientEnd().TakeChannel().release()));
  ASSERT_OK(zxio_close(&storage()->io));
  zxio_destroy(&storage()->io);
}

TEST_F(PacketSocketTest, Attr) {
  Init();

  zxio_node_attributes_t attr = {.has = {.object_type = true}};
  ASSERT_OK(zxio_attr_get(zxio(), &attr));
  EXPECT_EQ(ZXIO_OBJECT_TYPE_PACKET_SOCKET, attr.object_type);
}

}  // namespace
