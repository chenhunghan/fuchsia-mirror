// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/linux-initramfs/initramfs.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

using namespace std::literals;

using ::testing::ElementsAre;
using ::testing::Pair;

constexpr std::string_view kTestImage =
    "070701"sv  // magic
    "0CE447C8000041ED0002086000015F53000000026AB79FC9"sv
    "00000000"sv  // filesize
    "000000FD000000010000000000000000"sv
    "00000005"sv  // namesize
    "00000000"sv
    "dir1\0\0"sv
    "070701"sv  // magic
    "0CE447CA000081A40002086000015F53000000016AB79FC9"sv
    "0000000E"sv  // filesize
    "000000FD000000010000000000000000"sv
    "0000000B"sv  // namesize
    "00000000"sv
    "dir1/file1\0\0\0\0"sv
    "Hello, world!\n\0\0"sv
    "070701"sv  // magic
    "0CE447C9000041ED0002086000015F53000000026AB79FD2"sv
    "000000000"sv  // filesize
    "00000FD000000010000000000000000"sv
    "00000005"sv  // namesize
    "00000000"sv
    "dir2\0\0"sv
    "070701"sv  // magic
    "0CE447CB000081A40002086000015F53000000016AB79FD2"sv
    "0000000C"sv  // filesize
    "000000FD000000010000000000000000"sv
    "0000000B"sv  // namesize
    "00000000"sv
    "dir2/file2\0\0\0\0"sv
    "Lorem ipsum\n"sv
    "070701"sv  // magic
    "000000000000000000000000000000000000000100000000"sv
    "00000000"sv  // filesize
    "00000000000000000000000000000000"sv
    "0000000B"sv  // namesize
    "00000000"sv
    "TRAILER!!!\0\0\0\0"sv;
static_assert(kTestImage.size() == 632);

constexpr std::string_view kTestImage2 =
    "070701"sv  // magic
    "0CCA5C0D000081A40002086000015F53000000016ABB04CC"sv
    "0000000D"sv  // filesize
    "000000FD000000010000000000000000"sv
    "00000006"sv  // namesize
    "00000000file3\0"sv
    "Hello again!\n\0\0\0"sv
    "070701"sv  // magic
    "0CCA5C12000081A40002086000015F53000000016ABB04EA"sv
    "0000000F"sv  // filesize
    "000000FD000000010000000000000000"sv
    "00000006"sv  // namesize
    "00000000file4\0dolor sit amet\n\0"sv
    "070701"sv  // magic
    "070701"sv  // magic
    "000000000000000000000000000000000000000100000000"sv
    "00000000"sv  // filesize
    "00000000000000000000000000000000"sv
    "0000000B"sv  // namesize
    "00000000"sv
    "TRAILER!!!\0\0\0\0"sv;

TEST(InitramfsTests, Empty) {
  linux_initramfs::Initramfs ctor;  // Default constructed.
  EXPECT_TRUE(ctor.empty());

  linux_initramfs::Initramfs empty{std::span<std::byte>()};
  EXPECT_TRUE(empty.empty());

  ctor = empty;
  EXPECT_TRUE(empty.empty());
}

TEST(InitramfsTests, Truncated) {
  constexpr std::string_view kTrunacted = "0707010000000000";
  linux_initramfs::Initramfs truncated(kTrunacted);
  EXPECT_TRUE(truncated.empty());
}

TEST(InitramfsTests, Invalid) {
  const std::string bogus(200, 'x');
  linux_initramfs::Initramfs invalid(std::string_view{bogus});
  EXPECT_TRUE(invalid.empty());
}

TEST(InitramfsTests, InvalidWithCorrectMagic) {
  std::string bogus_with_magic = "070701";
  bogus_with_magic.resize(200, 'x');
  linux_initramfs::Initramfs magic_bogon(std::string_view{bogus_with_magic});
  EXPECT_TRUE(magic_bogon.empty());
}

TEST(InitramfsTests, Iterate) {
  constexpr auto as_str = [](const linux_initramfs::Member& member) {
    return std::string_view{reinterpret_cast<const char*>(member.contents.data()),
                            member.contents.size_bytes()};
  };

  linux_initramfs::Initramfs fs{kTestImage};
  EXPECT_FALSE(fs.empty());
  std::vector members{std::from_range, fs};
  ASSERT_EQ(members.size(), 4u);
  EXPECT_EQ(members[0].size_bytes(), 116u);
  EXPECT_EQ(members[0].name, "dir1");
  EXPECT_EQ(members[0].contents.size_bytes(), 0u);
  EXPECT_EQ(members[1].size_bytes(), 140u);
  EXPECT_EQ(members[1].name, "dir1/file1");
  EXPECT_EQ(as_str(members[1]), "Hello, world!\n");
  EXPECT_EQ(members[2].name, "dir2");
  EXPECT_EQ(members[2].size_bytes(), 116u);
  EXPECT_EQ(members[2].contents.size_bytes(), 0u);
  EXPECT_EQ(members[3].size_bytes(), 136u);
  EXPECT_EQ(members[3].name, "dir2/file2");
  EXPECT_EQ(as_str(members[3]), "Lorem ipsum\n");
}

TEST(InitramfsTests, Concatenate) {
  std::string image(kTestImage);
  static_assert(kTestImage.size() % 4 == 0);
  image += kTestImage2;

  linux_initramfs::Initramfs fs{image};
  constexpr auto as_pair = [](const linux_initramfs::Member& member) {
    return std::pair<std::string_view, std::string_view>{
        member.name,
        {
            reinterpret_cast<const char*>(member.contents.data()),
            member.contents.size(),
        },
    };
  };
  std::vector<std::pair<std::string_view, std::string_view>> members{
      std::from_range, std::views::transform(fs, as_pair)};
  EXPECT_THAT(members, ::testing::ElementsAre(                         //
                           Pair("dir1"sv, ""sv),                       //
                           Pair("dir1/file1"sv, "Hello, world!\n"sv),  //
                           Pair("dir2"sv, ""sv),                       //
                           Pair("dir2/file2"sv, "Lorem ipsum\n"sv),    //
                           Pair("file3"sv, "Hello again!\n"sv),        //
                           Pair("file4"sv, "dolor sit amet\n"sv)));
}

}  // namespace
