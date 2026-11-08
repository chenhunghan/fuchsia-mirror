// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fidl/fuchsia.virtualization.guest.interaction/cpp/fidl.h>
#include <fidl/fuchsia.virtualization/cpp/fidl.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <threads.h>

#include <map>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/lib/fxl/strings/trim.h"
#include "src/lib/testing/predicates/status.h"
#include "src/virtualization/lib/grpc/fdio_util.h"
#include "src/virtualization/lib/guest_interaction/client/client_impl.h"
#include "src/virtualization/lib/guest_interaction/common.h"
#include "src/virtualization/lib/guest_interaction/test/integration_test_lib.h"

namespace {

constexpr size_t kBufferSize = 100;

std::string drain_socket(zx::socket socket) {
  std::string out_string;
  size_t bytes_read;
  char read_buf[kBufferSize];

  while (true) {
    zx_status_t read_status = socket.read(0, read_buf, kBufferSize, &bytes_read);
    if (read_status == ZX_ERR_SHOULD_WAIT) {
      continue;
    }
    if (read_status != ZX_OK) {
      break;
    }
    out_string.append(read_buf, bytes_read);
  }

  return out_string;
}

static void ConvertSocketToNonBlockingFd(zx::socket socket, fbl::unique_fd& fd) {
  ASSERT_OK(fdio_fd_create(socket.release(), fd.reset_and_get_address()));
  int result;
  ASSERT_EQ((result = SetNonBlocking(fd)), 0) << strerror(result);
}

TEST_F(GuestInteractionTest, GrpcExecScriptTest) {
  // Connect the gRPC client to the guest under test.
  auto [ep_client, ep_server] =
      fidl::Endpoints<fuchsia_virtualization::HostVsockEndpoint>::Create();
  GetHostVsockEndpoint(std::move(ep_server));

  fidl::SyncClient ep(std::move(ep_client));
  fidl::Result<fuchsia_virtualization::HostVsockEndpoint::Connect> connect_result =
      ep->Connect({{.guest_port = GUEST_INTERACTION_PORT}});
  ASSERT_TRUE(connect_result.is_ok()) << connect_result.error_value().FormatDescription();

  fbl::unique_fd vsock_fd;
  ASSERT_NO_FATAL_FAILURE(
      ConvertSocketToNonBlockingFd(std::move(connect_result->socket()), vsock_fd));

  ClientImpl<PosixPlatform> client(vsock_fd.release());

  // Push the bash script to the guest.
  auto [put_client, put_server] = fidl::Endpoints<fuchsia_io::File>::Create();
  ASSERT_OK(fdio_open3(kTestScriptSource, static_cast<uint64_t>(fuchsia_io::wire::kPermReadable),
                       put_server.TakeChannel().release()));

  std::optional<zx_status_t> status;
  client.Put(std::move(put_client), kGuestScriptDestination,
             [&client, &status](zx_status_t put_result) {
               status = put_result;
               client.Stop();
             });
  client.Run();
  ASSERT_TRUE(status.has_value());
  ASSERT_OK(status.value());

  // Run the bash script in the guest.  The script will write to stdout and
  // stderr.  The script will also block waiting to receive input from stdin.
  zx::socket stdin_writer, stdin_reader;
  ASSERT_OK(zx::socket::create(0, &stdin_writer, &stdin_reader));

  zx::socket stdout_writer, stdout_reader;
  ASSERT_OK(zx::socket::create(0, &stdout_writer, &stdout_reader));

  zx::socket stderr_writer, stderr_reader;
  ASSERT_OK(zx::socket::create(0, &stderr_writer, &stderr_reader));

  // Once the subprocess has started, write to stdin.
  std::string to_write = kTestScriptInput;
  uint32_t bytes_written = 0;
  fbl::unique_fd stdin_fd;
  ASSERT_NO_FATAL_FAILURE(ConvertSocketToNonBlockingFd(std::move(stdin_writer), stdin_fd));

  // Run the bash script on the guest.
  std::string command = "/bin/sh ";
  command.append(kGuestScriptDestination);
  std::map<std::string, std::string> env_vars{{"STDOUT_STRING", kTestStdout},
                                              {"STDERR_STRING", kTestStderr}};
  std::string std_out;
  std::string std_err;
  std::optional<zx_status_t> exec_started;
  std::optional<zx_status_t> exec_terminated;
  int32_t ret_code = -1;

  auto listener_endpoints =
      fidl::Endpoints<fuchsia_virtualization_guest_interaction::CommandListener>::Create();

  struct ListenerHandler
      : public fidl::AsyncEventHandler<fuchsia_virtualization_guest_interaction::CommandListener> {
    ListenerHandler(ClientImpl<PosixPlatform>& client, fbl::unique_fd& stdin_fd,
                    const std::string& to_write, uint32_t& bytes_written, zx::socket& stdout_reader,
                    zx::socket& stderr_reader, std::optional<zx_status_t>& exec_started,
                    std::optional<zx_status_t>& exec_terminated, int32_t& ret_code,
                    std::string& std_out, std::string& std_err)
        : client_(client),
          stdin_fd_(stdin_fd),
          to_write_(to_write),
          bytes_written_(bytes_written),
          stdout_reader_(stdout_reader),
          stderr_reader_(stderr_reader),
          exec_started_(exec_started),
          exec_terminated_(exec_terminated),
          ret_code_(ret_code),
          std_out_(std_out),
          std_err_(std_err) {}

    void OnStarted(
        fidl::Event<fuchsia_virtualization_guest_interaction::CommandListener::OnStarted>& event)
        override {
      exec_started_ = event.status();
      if (event.status() == ZX_OK) {
        while (bytes_written_ < to_write_.size()) {
          ssize_t curr_bytes_written = write(stdin_fd_.get(), &(to_write_.c_str()[bytes_written_]),
                                             to_write_.size() - bytes_written_);
          if (curr_bytes_written < 0) {
            break;
          }
          bytes_written_ += static_cast<uint32_t>(curr_bytes_written);
        }
      }
      client_.Stop();
      stdin_fd_.reset();
    }

    void OnTerminated(
        fidl::Event<fuchsia_virtualization_guest_interaction::CommandListener::OnTerminated>& event)
        override {
      exec_terminated_ = event.status();
      ret_code_ = event.return_code();

      std_out_ = drain_socket(std::move(stdout_reader_));
      std_err_ = drain_socket(std::move(stderr_reader_));
      client_.Stop();
    }

    ClientImpl<PosixPlatform>& client_;
    fbl::unique_fd& stdin_fd_;
    const std::string& to_write_;
    uint32_t& bytes_written_;
    zx::socket& stdout_reader_;
    zx::socket& stderr_reader_;
    std::optional<zx_status_t>& exec_started_;
    std::optional<zx_status_t>& exec_terminated_;
    int32_t& ret_code_;
    std::string& std_out_;
    std::string& std_err_;
  } event_handler(client, stdin_fd, to_write, bytes_written, stdout_reader, stderr_reader,
                  exec_started, exec_terminated, ret_code, std_out, std_err);

  fidl::Client<fuchsia_virtualization_guest_interaction::CommandListener> listener(
      std::move(listener_endpoints.client), dispatcher(), &event_handler);

  client.Exec(command, env_vars, std::move(stdin_reader), std::move(stdout_writer),
              std::move(stderr_writer), std::move(listener_endpoints.server), dispatcher());

  // Ensure that the process started cleanly.
  {
    thrd_t client_run_thread;
    ASSERT_EQ(client.Start(client_run_thread), thrd_success);
    RunLoopUntil([&exec_started] { return exec_started.has_value(); });
    int32_t thread_ret_code;
    ASSERT_EQ(thrd_join(client_run_thread, &thread_ret_code), thrd_success);
    ASSERT_EQ(thread_ret_code, 0);
  }

  ASSERT_TRUE(exec_started.has_value());
  ASSERT_OK(exec_started.value());

  // Ensure the gRPC operation completed successfully and validate the stdout
  // and stderr.
  {
    thrd_t client_run_thread;
    ASSERT_EQ(client.Start(client_run_thread), thrd_success);
    RunLoopUntil([&exec_terminated] { return exec_terminated.has_value(); });
    int32_t thread_ret_code;
    ASSERT_EQ(thrd_join(client_run_thread, &thread_ret_code), thrd_success);
    ASSERT_EQ(thread_ret_code, 0);
  }

  ASSERT_TRUE(exec_terminated.has_value());
  ASSERT_OK(exec_terminated.value());
  ASSERT_EQ(fxl::TrimString(std_out, "\n"), std::string_view(kTestStdout));
  ASSERT_EQ(fxl::TrimString(std_err, "\n"), std::string_view(kTestStderr));

  // The bash script will create a file with contents that were written to
  // stdin.  Pull this file back and inspect its contents.
  auto [get_client, get_server] = fidl::Endpoints<fuchsia_io::File>::Create();
  ASSERT_OK(fdio_open3(
      kHostOuputCopyLocation,
      static_cast<uint64_t>(fuchsia_io::wire::kPermWritable | fuchsia_io::Flags::kFlagMaybeCreate |
                            fuchsia_io::Flags::kFileTruncate),
      get_server.TakeChannel().release()));

  status.reset();
  client.Get(kGuestFileOutputLocation, std::move(get_client), [&](zx_status_t get_result) {
    status = get_result;
    client.Stop();
  });
  client.Run();
  ASSERT_TRUE(status.has_value());
  ASSERT_OK(status.value());

  // Verify the contents that were communicated through stdin.
  std::string output_string;
  char output_buf[kBufferSize];
  fbl::unique_fd fd;
  ASSERT_TRUE(fd = fbl::unique_fd(open(kHostOuputCopyLocation, O_RDONLY))) << strerror(errno);
  while (true) {
    ssize_t read_size = read(fd.get(), output_buf, kBufferSize);
    ASSERT_GE(read_size, 0) << strerror(errno);
    if (read_size == 0) {
      break;
    }
    output_string.append(output_buf, read_size);
  }

  ASSERT_STREQ(output_string.c_str(), kTestScriptInput);
}

// Create a file large enough that it needs to be fragmented when it is sent
// to and received from the guest and then send it to and retrieve it from the
// guest.
TEST_F(GuestInteractionTest, GrpcPutGetTest) {
  // Connect the gRPC client to the guest under test.
  auto [ep_client, ep_server] =
      fidl::Endpoints<fuchsia_virtualization::HostVsockEndpoint>::Create();
  GetHostVsockEndpoint(std::move(ep_server));

  fidl::SyncClient ep(std::move(ep_client));
  fidl::Result<fuchsia_virtualization::HostVsockEndpoint::Connect> connect_result =
      ep->Connect({{.guest_port = GUEST_INTERACTION_PORT}});
  ASSERT_TRUE(connect_result.is_ok()) << connect_result.error_value().FormatDescription();

  fbl::unique_fd vsock_fd;
  ASSERT_NO_FATAL_FAILURE(
      ConvertSocketToNonBlockingFd(std::move(connect_result->socket()), vsock_fd));

  ClientImpl<PosixPlatform> client(vsock_fd.release());

  // Write a file of gibberish that the test can send over to the guest.
  constexpr char test_file[] = "/tmp/test_file.txt";
  constexpr char guest_destination[] = "/root/new/directory/test_file.txt";
  constexpr char host_verification_file[] = "/tmp/verification_file.txt";

  std::string file_contents;
  for (int i = 0; i < 2 * CHUNK_SIZE; i++) {
    file_contents.push_back(static_cast<char>(i % ('z' - 'A') + 'A'));
  }
  fbl::unique_fd fd;
  ASSERT_TRUE(fd = fbl::unique_fd(open(test_file, O_WRONLY | O_TRUNC | O_CREAT, S_IRUSR | S_IWUSR)))
      << strerror(errno);
  uint32_t bytes_written = 0;
  while (bytes_written < file_contents.size()) {
    ssize_t write_size = write(fd.get(), file_contents.c_str() + bytes_written,
                               file_contents.size() - bytes_written);
    ASSERT_GT(write_size, 0);
    bytes_written += write_size;
  }

  // Push the test file to the guest
  auto [put_client, put_server] = fidl::Endpoints<fuchsia_io::File>::Create();
  ASSERT_OK(fdio_open3(test_file, static_cast<uint64_t>(fuchsia_io::wire::kPermReadable),
                       put_server.TakeChannel().release()));

  std::optional<zx_status_t> status;
  client.Put(std::move(put_client), guest_destination, [&](zx_status_t put_result) {
    status = put_result;
    client.Stop();
  });
  client.Run();
  ASSERT_TRUE(status.has_value());
  ASSERT_OK(status.value());

  // Copy back the file that was sent to the guest.
  auto [get_client, get_server] = fidl::Endpoints<fuchsia_io::File>::Create();
  ASSERT_OK(fdio_open3(
      host_verification_file,
      static_cast<uint64_t>(fuchsia_io::wire::kPermWritable | fuchsia_io::Flags::kFlagMaybeCreate |
                            fuchsia_io::Flags::kFileTruncate),
      get_server.TakeChannel().release()));

  status.reset();
  client.Get(guest_destination, std::move(get_client), [&](zx_status_t get_result) {
    status = get_result;
    client.Stop();
  });
  client.Run();
  ASSERT_TRUE(status.has_value());
  ASSERT_OK(status.value());

  // Verify the contents that were communicated through stdin.
  std::string verification_string;
  char output_buf[kBufferSize];
  fd.reset(open(host_verification_file, O_RDONLY));
  while (true) {
    ssize_t read_size = read(fd.get(), output_buf, kBufferSize);
    ASSERT_GE(read_size, 0) << strerror(errno);
    if (read_size == 0) {
      break;
    }
    verification_string.append(std::string(output_buf, read_size));
  }

  ASSERT_EQ(verification_string, file_contents);
}

}  // namespace
