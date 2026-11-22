// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/server.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "src/developer/debug/shared/logging/logging.h"
#include "src/developer/debug/zxdb/client/session.h"

namespace zxdb {
namespace {

// Binds |fd| to the loopback address for |family| (AF_INET or AF_INET6) on |port|.
// Sets |unavailable| to true without error when |family| has no loopback address configured.
Err BindLoopbackAddress(int fd, int family, uint16_t port, bool* unavailable) {
  *unavailable = false;
  sockaddr_storage storage{};
  socklen_t addr_len = 0;
  if (family == AF_INET6) {
    // Restrict the IPv6 socket to IPv6 traffic so IPv4 loopback binds separately.
    const int v6only = 1;
    if (setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &v6only, sizeof(v6only)) < 0) {
      return Err("Could not set IPV6_V6ONLY (errno=%d: %s).", errno, strerror(errno));
    }
    sockaddr_in6 addr{};
    addr.sin6_family = AF_INET6;
    addr.sin6_addr = in6addr_loopback;
    addr.sin6_port = htons(port);
    addr_len = sizeof(addr);
    memcpy(&storage, &addr, addr_len);
  } else {
    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(port);
    addr_len = sizeof(addr);
    memcpy(&storage, &addr, addr_len);
  }

  if (bind(fd, reinterpret_cast<const sockaddr*>(&storage), addr_len) < 0) {
    if (errno == EADDRNOTAVAIL) {
      *unavailable = true;
      return Err();
    }
    return Err("Could not bind loopback socket to port %u (errno=%d: %s).", port, errno,
               strerror(errno));
  }
  return Err();
}

// Creates, binds, and listens on a loopback TCP socket for |family| (AF_INET or AF_INET6).
// Leaves |out_fd| invalid and returns an ok Err when |family| is unavailable on this host.
Err CreateLoopbackSocket(int family, uint16_t port, fbl::unique_fd* out_fd) {
  fbl::unique_fd fd(socket(family, SOCK_STREAM, IPPROTO_TCP));
  if (!fd.is_valid()) {
    if (errno == EAFNOSUPPORT || errno == EPROTONOSUPPORT) {
      return Err();
    }
    return Err("Could not create socket (errno=%d: %s).", errno, strerror(errno));
  }

  const int reuse = 1;
  if (setsockopt(fd.get(), SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)) < 0) {
    return Err("Could not set SO_REUSEADDR (errno=%d: %s).", errno, strerror(errno));
  }

  bool unavailable = false;
  if (Err err = BindLoopbackAddress(fd.get(), family, port, &unavailable); err.has_error()) {
    return err;
  }
  if (unavailable) {
    return Err();
  }

  if (listen(fd.get(), 1) < 0) {
    return Err("Failed to listen on server socket (errno=%d: %s).", errno, strerror(errno));
  }

  *out_fd = std::move(fd);
  return Err();
}

}  // namespace

DebugAdapterServer::DebugAdapterServer(Console* console, uint16_t port)
    : console_(console), port_(port) {
  int fd[2] = {};
  pipe(fd);
  exit_pipe_[0] = fbl::unique_fd(fd[0]);
  exit_pipe_[1] = fbl::unique_fd(fd[1]);
}

Err DebugAdapterServer::Init() {
  main_loop_ = debug::MessageLoop::Current();

  // Bind only to IPv4 and IPv6 loopback addresses.
  for (int family : {AF_INET6, AF_INET}) {
    fbl::unique_fd fd;
    if (Err err = CreateLoopbackSocket(family, port_, &fd); err.has_error()) {
      server_sockets_.clear();
      return err;
    }
    if (fd.is_valid()) {
      server_sockets_.push_back(std::move(fd));
    }
  }

  if (server_sockets_.empty()) {
    return Err("Could not bind any loopback listening socket on port %u.", port_);
  }

  ListenConnection();
  return Err();
}

void DebugAdapterServer::ListenConnection() {
  FX_DCHECK(!background_thread_.get());  // Duplicate ListenConnection() call.

  // Create the background thread to listen to incoming connections.
  background_thread_ = std::make_unique<std::thread>([this]() { ListenBackgroundThread(); });
}

void DebugAdapterServer::ListenBackgroundThread() {
  // Wait for one connection.
  LOGS(Info) << "Waiting on port " << port_ << " for debug adapter connection.";
  fbl::unique_fd client;
  while (!Accept(client)) {
    if (background_thread_exit_) {
      return;
    }
  }
  LOGS(Info) << "Debug Adapter connection established.";

  main_loop_->PostTask(FROM_HERE, [this, client = std::move(client)]() mutable {
    ConnectionResolvedMainThread(std::move(client));
  });
}

bool DebugAdapterServer::Accept(fbl::unique_fd& client) {
  // Wait on exit_pipe_ and all listening sockets until a connection arrives or exit is requested.
  std::vector<pollfd> fds;
  fds.reserve(1 + server_sockets_.size());
  fds.push_back({.fd = exit_pipe_[0].get(), .events = POLLIN, .revents = 0});
  for (const auto& sock : server_sockets_) {
    fds.push_back({.fd = sock.get(), .events = POLLIN, .revents = 0});
  }

  int status = 0;
  do {
    status = poll(fds.data(), static_cast<nfds_t>(fds.size()), -1);
  } while (status < 0 && errno == EINTR);
  if (status <= 0) {
    return false;
  }

  constexpr unsigned int kExitPollMask = static_cast<unsigned int>(POLLIN) |
                                         static_cast<unsigned int>(POLLHUP) |
                                         static_cast<unsigned int>(POLLERR);
  if ((static_cast<unsigned int>(fds[0].revents) & kExitPollMask) != 0) {
    return false;
  }

  for (size_t i = 1; i < fds.size(); ++i) {
    if ((static_cast<unsigned int>(fds[i].revents) & static_cast<unsigned int>(POLLIN)) == 0) {
      continue;
    }

    sockaddr_storage addr{};
    socklen_t addrlen = sizeof(addr);
    client = fbl::unique_fd(accept(fds[i].fd, reinterpret_cast<sockaddr*>(&addr), &addrlen));
    if (!client.is_valid()) {
      LOGS(Error) << "Accept failed: " << strerror(errno);
      continue;
    }

    if (fcntl(client.get(), F_SETFL, O_NONBLOCK) < 0) {
      LOGS(Error) << "Couldn't make port nonblocking.";
      client.reset();
      continue;
    }
    return true;
  }

  return false;
}

void DebugAdapterServer::ConnectionResolvedMainThread(fbl::unique_fd client) {
  background_thread_->join();
  background_thread_.reset();

  for (auto& observer : observers_) {
    observer.ClientConnected();
  }

  buffer_ = std::make_unique<debug::BufferedFD>(std::move(client));
  if (!buffer_->Start()) {
    LOGS(Error) << "Failed to initialize debug adapter buffer";
    return;
  }

  context_ = std::make_unique<DebugAdapterContext>(console_, &buffer_->stream());
  buffer_->set_data_available_callback(
      [context = context_.get()]() { context->OnStreamReadable(); });
  context_->set_destroy_connection_callback([this]() { context_->console()->Quit(); });

  // Reset the client connection on error.
  buffer_->set_error_callback([this]() {
    main_loop_->PostTask(FROM_HERE, [this]() {
      LOGS(Info) << "Connection lost.";
      OnDisconnect();
    });
  });
}

// Connect again if the IDE client disconnects (we are expecting 1 disconnection).
// TODO(https://fxbug.dev/42067532): Differentiate behavior between use cases.
void DebugAdapterServer::OnDisconnect() {
  ResetClientConnection();
  for (auto& observer : observers_) {
    observer.ClientDisconnected();
  }
  ListenConnection();
}

void DebugAdapterServer::ResetClientConnection() {
  context_.reset();
  buffer_.reset();
}

DebugAdapterServer::~DebugAdapterServer() {
  ResetClientConnection();

  if (background_thread_) {
    background_thread_exit_ = true;
    // Write to exit_pipe_ to unblock poll().
    ssize_t ret;
    do {
      int val = 1;
      ret = write(exit_pipe_[1].get(), &val, sizeof(val));
    } while (ret < 0 && errno == EINTR);
    // Wait for background thread to exit.
    background_thread_->join();
    background_thread_.reset();
  }
}

}  // namespace zxdb
