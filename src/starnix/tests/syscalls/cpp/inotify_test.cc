// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

TEST(InotifyTest, DeleteWatchedDirWithoutDeleteSelfMaskEmitsIgnored) {
  test_helper::ScopedTempDir temp_dir;
  std::string watched_dir = temp_dir.path() + "/watched";
  ASSERT_EQ(mkdir(watched_dir.c_str(), 0700), 0) << strerror(errno);

  fbl::unique_fd fd(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  int wd = inotify_add_watch(fd.get(), watched_dir.c_str(), IN_CREATE | IN_DELETE);
  ASSERT_GE(wd, 0) << strerror(errno);

  ASSERT_EQ(rmdir(watched_dir.c_str()), 0) << strerror(errno);

  struct inotify_event event = {};
  ssize_t bytes_read = read(fd.get(), &event, sizeof(event));
  ASSERT_EQ(bytes_read, static_cast<ssize_t>(sizeof(event))) << strerror(errno);
  EXPECT_EQ(event.wd, wd);
  EXPECT_EQ(event.mask, static_cast<uint32_t>(IN_IGNORED));

  // No further events should be queued, and the watch descriptor should already be removed.
  EXPECT_EQ(read(fd.get(), &event, sizeof(event)), -1);
  EXPECT_EQ(errno, EAGAIN);
  EXPECT_EQ(inotify_rm_watch(fd.get(), wd), -1);
  EXPECT_EQ(errno, EINVAL);
}

TEST(InotifyTest, DeleteWatchedDirWithDeleteSelfMaskEmitsDeleteSelfAndIgnored) {
  test_helper::ScopedTempDir temp_dir;
  std::string watched_dir = temp_dir.path() + "/watched";
  ASSERT_EQ(mkdir(watched_dir.c_str(), 0700), 0) << strerror(errno);

  fbl::unique_fd fd(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  int wd = inotify_add_watch(fd.get(), watched_dir.c_str(), IN_CREATE | IN_DELETE_SELF);
  ASSERT_GE(wd, 0) << strerror(errno);

  ASSERT_EQ(rmdir(watched_dir.c_str()), 0) << strerror(errno);

  struct inotify_event events[2] = {};
  ssize_t bytes_read = read(fd.get(), events, sizeof(events));
  ASSERT_EQ(bytes_read, static_cast<ssize_t>(sizeof(events))) << strerror(errno);
  EXPECT_EQ(events[0].wd, wd);
  EXPECT_EQ(events[0].mask, static_cast<uint32_t>(IN_DELETE_SELF));
  EXPECT_EQ(events[1].wd, wd);
  EXPECT_EQ(events[1].mask, static_cast<uint32_t>(IN_IGNORED));

  EXPECT_EQ(inotify_rm_watch(fd.get(), wd), -1);
  EXPECT_EQ(errno, EINVAL);
}

// Linux queues the child's self-events before the parent's IN_DELETE.
TEST(InotifyTest, DeleteWatchedDirEmitsSelfEventsBeforeParentDelete) {
  test_helper::ScopedTempDir temp_dir;
  std::string watched_dir = temp_dir.path() + "/subdir";
  ASSERT_EQ(mkdir(watched_dir.c_str(), 0700), 0) << strerror(errno);

  fbl::unique_fd fd(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);

  int wd_parent = inotify_add_watch(fd.get(), temp_dir.path().c_str(), IN_DELETE);
  ASSERT_GE(wd_parent, 0) << strerror(errno);
  int wd_child = inotify_add_watch(fd.get(), watched_dir.c_str(), IN_DELETE_SELF);
  ASSERT_GE(wd_child, 0) << strerror(errno);

  ASSERT_EQ(rmdir(watched_dir.c_str()), 0) << strerror(errno);

  alignas(struct inotify_event) char buf[4096] = {};
  ssize_t bytes_read = read(fd.get(), buf, sizeof(buf));
  ASSERT_GE(bytes_read, static_cast<ssize_t>(3 * sizeof(struct inotify_event))) << strerror(errno);

  size_t offset = 0;
  auto* ev0 = reinterpret_cast<struct inotify_event*>(buf + offset);
  EXPECT_EQ(ev0->wd, wd_child);
  EXPECT_EQ(ev0->mask, static_cast<uint32_t>(IN_DELETE_SELF));
  offset += sizeof(struct inotify_event) + ev0->len;
  ASSERT_LE(offset + sizeof(struct inotify_event), static_cast<size_t>(bytes_read));

  auto* ev1 = reinterpret_cast<struct inotify_event*>(buf + offset);
  EXPECT_EQ(ev1->wd, wd_child);
  EXPECT_EQ(ev1->mask, static_cast<uint32_t>(IN_IGNORED));
  offset += sizeof(struct inotify_event) + ev1->len;
  ASSERT_LE(offset + sizeof(struct inotify_event), static_cast<size_t>(bytes_read));

  auto* ev2 = reinterpret_cast<struct inotify_event*>(buf + offset);
  EXPECT_EQ(ev2->wd, wd_parent);
  EXPECT_EQ(ev2->mask, static_cast<uint32_t>(IN_DELETE | IN_ISDIR));
  ASSERT_GT(ev2->len, 0u);
  ASSERT_LE(offset + sizeof(struct inotify_event) + ev2->len, static_cast<size_t>(bytes_read));
  ASSERT_LT(strnlen(ev2->name, ev2->len), ev2->len);
  EXPECT_STREQ(ev2->name, "subdir");
  offset += sizeof(struct inotify_event) + ev2->len;

  EXPECT_EQ(offset, static_cast<size_t>(bytes_read));
}

// Linux emits IN_DELETE_SELF from fsnotify_inoderemove(), which runs only once the inode has no
// links left, so unlinking one of several hard links reports just the link count change and leaves
// the watch in place until the final link is removed.
void CheckHardLinkUnlinkOrder(bool watch_first) {
  test_helper::ScopedTempDir temp_dir;
  std::string first = temp_dir.path() + "/first";
  std::string second = temp_dir.path() + "/second";

  fbl::unique_fd created(open(first.c_str(), O_CREAT | O_RDWR, 0600));
  ASSERT_TRUE(created.is_valid()) << strerror(errno);
  created.reset();
  ASSERT_EQ(link(first.c_str(), second.c_str()), 0) << strerror(errno);

  fbl::unique_fd fd(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
  ASSERT_TRUE(fd.is_valid()) << strerror(errno);
  const std::string& watched = watch_first ? first : second;
  int wd = inotify_add_watch(fd.get(), watched.c_str(), IN_ATTRIB | IN_DELETE_SELF);
  ASSERT_GE(wd, 0) << strerror(errno);

  ASSERT_EQ(unlink(first.c_str()), 0) << strerror(errno);

  struct inotify_event event = {};
  ASSERT_EQ(read(fd.get(), &event, sizeof(event)), static_cast<ssize_t>(sizeof(event)))
      << strerror(errno);
  EXPECT_EQ(event.wd, wd);
  EXPECT_EQ(event.mask, static_cast<uint32_t>(IN_ATTRIB));
  EXPECT_EQ(read(fd.get(), &event, sizeof(event)), -1);
  EXPECT_EQ(errno, EAGAIN);

  // Unlinking the remaining link destroys the inode and emits IN_ATTRIB, IN_DELETE_SELF, and
  // IN_IGNORED.
  ASSERT_EQ(unlink(second.c_str()), 0) << strerror(errno);

  struct inotify_event final_events[3] = {};
  ASSERT_EQ(read(fd.get(), final_events, sizeof(final_events)),
            static_cast<ssize_t>(sizeof(final_events)))
      << strerror(errno);
  EXPECT_EQ(final_events[0].wd, wd);
  EXPECT_EQ(final_events[0].mask, static_cast<uint32_t>(IN_ATTRIB));
  EXPECT_EQ(final_events[1].wd, wd);
  EXPECT_EQ(final_events[1].mask, static_cast<uint32_t>(IN_DELETE_SELF));
  EXPECT_EQ(final_events[2].wd, wd);
  EXPECT_EQ(final_events[2].mask, static_cast<uint32_t>(IN_IGNORED));

  EXPECT_EQ(inotify_rm_watch(fd.get(), wd), -1);
  EXPECT_EQ(errno, EINVAL);
}

TEST(InotifyTest, UnlinkOneOfTwoHardLinksDoesNotEmitDeleteSelf) {
  ASSERT_NO_FATAL_FAILURE(CheckHardLinkUnlinkOrder(/*watch_first=*/false));
}

// TODO(https://fxbug.dev/565365826): Unlinking the watched link first currently pins its
// `DirEntryHandle` in `InotifyState::watches`, preventing `DELETE_SELF` when the surviving link is
// unlinked.
TEST(InotifyTest, UnlinkWatchedHardLinkFirstEmitsDeleteSelfOnLastUnlink) {
  ASSERT_NO_FATAL_FAILURE(CheckHardLinkUnlinkOrder(/*watch_first=*/true));
}

}  // namespace
