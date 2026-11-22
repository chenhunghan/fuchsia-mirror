// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/selinux/userspace/audit_utils.h"

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstring>
#include <deque>
#include <fstream>
#include <iterator>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/audit.h>
#include <linux/netlink.h>

#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

constexpr int NETLINK_BUF_SIZE = 4096;

struct audit_request {
  struct nlmsghdr nlh;
  audit_utils::audit_status status;
};

// Helper function to open a Netlink socket for Audit
fbl::unique_fd OpenNetlinkAuditSocket() {
  fbl::unique_fd fd(socket(AF_NETLINK, SOCK_RAW, NETLINK_AUDIT));
  return fd;
}

// Helper function to send a netlink message
fit::result<int, ssize_t> SendNetlinkMessage(int fd, const void* buf, size_t len) {
  ssize_t sent = send(fd, buf, len, 0);
  if ((size_t)sent != len) {
    return fit::error(errno);
  }
  return fit::ok(sent);
}

// Helper function to receive a netlink message
fit::result<int, ssize_t> ReceiveNetlinkMessage(int fd, void* buf, size_t len, bool wait) {
  ssize_t received = recv(fd, buf, len, wait ? 0 : MSG_DONTWAIT);
  if (received < 0) {
    return fit::error(errno);
  }
  return fit::ok(received);
}

fit::result<int, ssize_t> ReceiveNetlinkMessage(int fd,
                                                std::deque<std::vector<char>>& buffered_messages,
                                                void* buf, size_t len, bool wait) {
  if (!buffered_messages.empty()) {
    const auto& msg = buffered_messages.front();
    size_t copy_len = std::min(len, msg.size());
    if (copy_len > 0) {
      memcpy(buf, msg.data(), copy_len);
    }
    buffered_messages.pop_front();
    return fit::ok(static_cast<ssize_t>(copy_len));
  }
  return ReceiveNetlinkMessage(fd, buf, len, wait);
}

// Helper to register the calling process as the audit daemon on a given socket.
fit::result<int> RegisterAsAuditDaemon(int fd, pid_t pid,
                                       std::deque<std::vector<char>>* buffered_messages) {
  audit_request req;
  char buf[NETLINK_BUF_SIZE]{};

  memset(&req, 0, sizeof(req));
  req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.status));
  req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
  req.nlh.nlmsg_seq = 1;
  req.nlh.nlmsg_type = AUDIT_SET;
  req.status.pid = pid;
  req.status.mask = AUDIT_STATUS_PID;

  auto send_res = SendNetlinkMessage(fd, &req, req.nlh.nlmsg_len);
  if (send_res.is_error()) {
    return send_res.take_error();
  }

  struct nlmsghdr* nlh;
  ssize_t bytes_received = 0;
  std::deque<std::vector<char>> pending;
  do {
    auto recv_res = ReceiveNetlinkMessage(fd, buf, sizeof(buf));
    if (recv_res.is_error()) {
      return recv_res.take_error();
    }
    if (recv_res.value() < NLMSG_HDRLEN) {
      return fit::error(EINVAL);
    }
    bytes_received = recv_res.value();
    nlh = reinterpret_cast<struct nlmsghdr*>(buf);
    // Preserve non-ACK audit messages flushed onto `fd` once the kernel associates it with `pid`
    // before the `NLMSG_ERROR` ACK is dequeued.
    if (nlh->nlmsg_type != NLMSG_ERROR && buffered_messages != nullptr) {
      size_t msg_len = std::min(static_cast<size_t>(bytes_received), sizeof(buf));
      pending.emplace_back(buf, buf + msg_len);
    }
  } while (nlh->nlmsg_type != NLMSG_ERROR);

  if (bytes_received < static_cast<ssize_t>(NLMSG_LENGTH(sizeof(struct nlmsgerr)))) {
    return fit::error(EBADMSG);
  }
  auto* err = reinterpret_cast<struct nlmsgerr*>(NLMSG_DATA(nlh));
  if (err->error) {
    return fit::error(-err->error);
  }
  if (buffered_messages != nullptr) {
    buffered_messages->insert(buffered_messages->end(), std::make_move_iterator(pending.begin()),
                              std::make_move_iterator(pending.end()));
  }
  return fit::ok();
}

// Helper to unregister the current audit daemon.
fit::result<int> UnregisterAuditDaemon(int fd) { return RegisterAsAuditDaemon(fd, /*pid=*/0); }

// Helper to query the current audit status from the kernel via AUDIT_GET.
fit::result<int, audit_utils::audit_status> GetAuditStatus(int fd) {
  struct nlmsghdr req{};
  req.nlmsg_len = NLMSG_LENGTH(0);
  req.nlmsg_flags = NLM_F_REQUEST;
  req.nlmsg_seq = 1;
  req.nlmsg_type = AUDIT_GET;

  auto send_res = SendNetlinkMessage(fd, &req, req.nlmsg_len);
  if (send_res.is_error()) {
    return send_res.take_error();
  }

  char buf[NETLINK_BUF_SIZE]{};
  struct nlmsghdr* nlh;
  ssize_t bytes_received = 0;
  do {
    auto recv_res = ReceiveNetlinkMessage(fd, buf, sizeof(buf));
    if (recv_res.is_error()) {
      return recv_res.take_error();
    }
    if (recv_res.value() < static_cast<ssize_t>(NLMSG_LENGTH(0))) {
      return fit::error(EINVAL);
    }
    bytes_received = recv_res.value();
    nlh = reinterpret_cast<struct nlmsghdr*>(buf);
    if (nlh->nlmsg_type == NLMSG_ERROR) {
      if (recv_res.value() < static_cast<ssize_t>(NLMSG_LENGTH(sizeof(struct nlmsgerr)))) {
        return fit::error(EBADMSG);
      }
      auto* err = reinterpret_cast<struct nlmsgerr*>(NLMSG_DATA(nlh));
      return fit::error(-err->error);
    }
  } while (nlh->nlmsg_type != AUDIT_GET);
  if (nlh->nlmsg_len < NLMSG_SPACE(0) || bytes_received < static_cast<ssize_t>(nlh->nlmsg_len)) {
    return fit::error(EBADMSG);
  }
  constexpr size_t kMinAuditStatusSize =
      sizeof(audit_utils::audit_status::mask) + sizeof(audit_utils::audit_status::enabled) +
      sizeof(audit_utils::audit_status::failure) + sizeof(audit_utils::audit_status::pid) +
      sizeof(audit_utils::audit_status::rate_limit) +
      sizeof(audit_utils::audit_status::backlog_limit) + sizeof(audit_utils::audit_status::lost) +
      sizeof(audit_utils::audit_status::backlog);
  if (NLMSG_PAYLOAD(nlh, 0) < kMinAuditStatusSize) {
    return fit::error(EBADMSG);
  }

  audit_utils::audit_status status{};
  memcpy(&status, NLMSG_DATA(nlh),
         std::min(static_cast<size_t>(NLMSG_PAYLOAD(nlh, 0)), sizeof(status)));
  return fit::ok(status);
}

// Helper to send a user-space audit message (e.g., from an AVC denial).
fit::result<int> SendUserAuditMessage(int fd, int seq, const std::string& message_text, bool ack) {
  const size_t payload_len = message_text.length() + 1;
  const size_t buf_len = NLMSG_SPACE(payload_len);
  std::vector<char> buf(buf_len);
  char ack_buf[NETLINK_BUF_SIZE]{};
  memset(buf.data(), 0, buf_len);

  auto* nlh = reinterpret_cast<struct nlmsghdr*>(buf.data());
  nlh->nlmsg_len = NLMSG_LENGTH((int)payload_len);
  nlh->nlmsg_type = AUDIT_USER_AVC;
  nlh->nlmsg_flags = NLM_F_REQUEST;
  if (ack) {
    nlh->nlmsg_flags |= NLM_F_ACK;
  }
  nlh->nlmsg_seq = seq;

  char* data = static_cast<char*>(NLMSG_DATA(nlh));
  strncpy(data, message_text.c_str(), payload_len);

  auto send_res = SendNetlinkMessage(fd, nlh, nlh->nlmsg_len);
  if (send_res.is_error()) {
    return send_res.take_error();
  }
  if (!ack) {
    return fit::ok();
  }

  // Now, wait for the acknowledgment from the kernel.
  auto recv_res = ReceiveNetlinkMessage(fd, ack_buf, sizeof(ack_buf));
  if (recv_res.is_error()) {
    return recv_res.take_error();
  }

  if (recv_res.value() < NLMSG_HDRLEN) {
    return fit::error(EINVAL);
  }

  auto* ack_nlh = reinterpret_cast<struct nlmsghdr*>(ack_buf);
  if (ack_nlh->nlmsg_type != NLMSG_ERROR) {
    return fit::error(EBADMSG);
  }

  if (recv_res.value() < static_cast<ssize_t>(NLMSG_LENGTH(sizeof(struct nlmsgerr)))) {
    return fit::error(EBADMSG);
  }

  auto* err = reinterpret_cast<struct nlmsgerr*>(NLMSG_DATA(ack_nlh));
  if (err->error) {
    return fit::error(-err->error);
  }
  return fit::ok();
}
