// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <elf.h>
#include <fcntl.h>
#include <sys/fsuid.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#include <fbl/unique_fd.h>

#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

constexpr int kOutputFd = 100;
constexpr uid_t kTestUid = 65533;
constexpr gid_t kTestGid = 65534;

std::string GetCredsBinaryPath() {
  return test_helper::GetTestResourcePath("print_uid_gid_exec_child");
}

#if defined(__LP64__)
using ElfEhdr = Elf64_Ehdr;
using ElfPhdr = Elf64_Phdr;
constexpr uint8_t kElfClass = ELFCLASS64;
#else
using ElfEhdr = Elf32_Ehdr;
using ElfPhdr = Elf32_Phdr;
constexpr uint8_t kElfClass = ELFCLASS32;
#endif

#if defined(__x86_64__)
constexpr uint16_t kElfMachine = EM_X86_64;
#elif defined(__i386__)
constexpr uint16_t kElfMachine = EM_386;
#elif defined(__aarch64__)
constexpr uint16_t kElfMachine = EM_AARCH64;
#elif defined(__arm__)
constexpr uint16_t kElfMachine = EM_ARM;
#elif defined(__riscv)
constexpr uint16_t kElfMachine = EM_RISCV;
#endif

test_helper::ScopedTempFD CreateTestElf(const std::vector<ElfPhdr> &phdrs) {
  ElfEhdr ehdr = {};
  ehdr.e_ident[EI_MAG0] = ELFMAG0;
  ehdr.e_ident[EI_MAG1] = ELFMAG1;
  ehdr.e_ident[EI_MAG2] = ELFMAG2;
  ehdr.e_ident[EI_MAG3] = ELFMAG3;
  ehdr.e_ident[EI_CLASS] = kElfClass;
  ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  ehdr.e_type = ET_EXEC;
  ehdr.e_machine = kElfMachine;
  ehdr.e_version = EV_CURRENT;
  ehdr.e_phoff = sizeof(ehdr);
  ehdr.e_ehsize = sizeof(ehdr);
  ehdr.e_phentsize = sizeof(ElfPhdr);
  ehdr.e_phnum = static_cast<uint16_t>(phdrs.size());

  test_helper::ScopedTempFD temp_file;
  EXPECT_TRUE(temp_file.is_valid());
  SAFE_SYSCALL(fchmod(temp_file.fd(), 0755));
  EXPECT_EQ(write(temp_file.fd(), &ehdr, sizeof(ehdr)), static_cast<ssize_t>(sizeof(ehdr)));
  if (!phdrs.empty()) {
    const size_t phdrs_bytes = phdrs.size() * sizeof(ElfPhdr);
    EXPECT_EQ(write(temp_file.fd(), phdrs.data(), phdrs_bytes), static_cast<ssize_t>(phdrs_bytes));
  }
  temp_file.fd_.reset();
  return temp_file;
}

}  // namespace

TEST(ExecTest, FsuidFsgidResetOnExec) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  std::string creds_binary = GetCredsBinaryPath();

  int fd = SAFE_SYSCALL(test_helper::MemFdCreate("creds", O_RDWR));

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    SAFE_SYSCALL(dup2(fd, kOutputFd));

    // We start as root (ruid=0, euid=0, fsuid=0).
    // We want to set euid to kTestUid, but keep fsuid as 0.
    // This allows us to execute the root-owned (700) helper binary,
    // while still testing that fsuid is reset to euid (kTestUid) on exec.

    SAFE_SYSCALL(setegid(kTestGid));
    SAFE_SYSCALL(seteuid(kTestUid));

    // seteuid/setegid also set fsuid/fsgid to the new euid/egid.
    // We explicitly set them back to 0 (root).
    // This is allowed because our real UID/GID are still 0.
    ASSERT_EQ(setfsuid(0), static_cast<int>(kTestUid));
    ASSERT_EQ(setfsgid(0), static_cast<int>(kTestGid));

    // Verify the state before exec.
    uid_t ruid, euid, suid;
    SAFE_SYSCALL(getresuid(&ruid, &euid, &suid));
    ASSERT_EQ(ruid, 0U);
    ASSERT_EQ(euid, kTestUid);

    gid_t rgid, egid, sgid;
    SAFE_SYSCALL(getresgid(&rgid, &egid, &sgid));
    ASSERT_EQ(rgid, 0U);
    ASSERT_EQ(egid, kTestGid);

    ASSERT_EQ(setfsuid(-1), 0);
    ASSERT_EQ(setfsgid(-1), 0);

    char *const argv[] = {const_cast<char *>(creds_binary.c_str()), nullptr};
    execve(creds_binary.c_str(), argv, nullptr);
    perror("execve");
    _exit(EXIT_FAILURE);
  });

  ASSERT_TRUE(helper.WaitForChildren());

  SAFE_SYSCALL(lseek(fd, 0, SEEK_SET));
  FILE *fp = fdopen(fd, "r");
  ASSERT_NE(fp, nullptr);

  uid_t ruid, euid, suid;
  EXPECT_EQ(fscanf(fp, "ruid: %u euid: %u suid: %u\n", &ruid, &euid, &suid), 3);
  EXPECT_EQ(ruid, 0U);
  EXPECT_EQ(euid, kTestUid);

  gid_t rgid, egid, sgid;
  EXPECT_EQ(fscanf(fp, "rgid: %u egid: %u sgid: %u\n", &rgid, &egid, &sgid), 3);
  EXPECT_EQ(rgid, 0U);
  EXPECT_EQ(egid, kTestGid);

  int fsuid, fsgid;
  EXPECT_EQ(fscanf(fp, "fsuid: %d fsgid: %d\n", &fsuid, &fsgid), 2);
  // fsuid/fsgid should have been reset to euid/egid (kTestUid/kTestGid) on exec,
  // even though they were 0 before exec.
  EXPECT_EQ(fsuid, static_cast<int>(kTestUid));
  EXPECT_EQ(fsgid, static_cast<int>(kTestGid));

  fclose(fp);
}

// An ELF with zero program headers (e_phnum == 0) is rejected with ENOEXEC.
TEST(ExecTest, ElfWithNoProgramHeaders) {
  test_helper::ScopedTempFD temp_file = CreateTestElf({});

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    char *const argv[] = {const_cast<char *>(temp_file.name().c_str()), nullptr};
    char *const envp[] = {nullptr};
    EXPECT_THAT(execve(temp_file.name().c_str(), argv, envp), SyscallFailsWithErrno(ENOEXEC));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

// An ELF with a non-empty program header table (e_phnum == 1, PT_NULL) but zero
// PT_LOAD segments succeeds in replacing the process image with no mapped executable
// segments and faults with SIGSEGV upon jumping to e_entry (0x0).
TEST(ExecTest, ElfWithNoLoadSegments) {
  ElfPhdr null_phdr = {};
  null_phdr.p_type = PT_NULL;
  test_helper::ScopedTempFD temp_file = CreateTestElf({null_phdr});

  test_helper::ForkHelper helper;
  helper.ExpectSignal(SIGSEGV);
  helper.RunInForkedProcess([&] {
    char *const argv[] = {const_cast<char *>(temp_file.name().c_str()), nullptr};
    char *const envp[] = {nullptr};
    execve(temp_file.name().c_str(), argv, envp);
    ADD_FAILURE() << "execve unexpectedly returned: " << strerror(errno);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

// An ELF whose PT_LOAD segment has p_offset not congruent with p_vaddr modulo
// the page size fails while mapping segments after tearing down the old address
// space, terminating the process with SIGSEGV.
TEST(ExecTest, ElfWithUnalignedLoadOffset) {
  ElfPhdr load_phdr = {};
  load_phdr.p_type = PT_LOAD;
  load_phdr.p_flags = PF_R | PF_X;
  load_phdr.p_offset = 1;
  load_phdr.p_vaddr = 0x20000000;
  load_phdr.p_filesz = sizeof(ElfEhdr);
  load_phdr.p_memsz = sizeof(ElfEhdr);
  test_helper::ScopedTempFD temp_file = CreateTestElf({load_phdr});

  test_helper::ForkHelper helper;
  helper.ExpectSignal(SIGSEGV);
  helper.RunInForkedProcess([&] {
    char *const argv[] = {const_cast<char *>(temp_file.name().c_str()), nullptr};
    char *const envp[] = {nullptr};
    execve(temp_file.name().c_str(), argv, envp);
    ADD_FAILURE() << "execve unexpectedly returned: " << strerror(errno);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}
