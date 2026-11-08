// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <elf.h>
#include <fcntl.h>
#include <inttypes.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdio/io.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <zircon/dlfcn.h>
#include <zircon/processargs.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <limits>
#include <vector>

#include <zxtest/zxtest.h>

#include "src/lib/loader_service/loader_service.h"

namespace {

#ifndef LIBPREFIX
#error "BUILD.gn should define LIBPREFIX"
#endif

zx_status_t load_vmo(const char* filename, zx_handle_t* out) {
  int fd = open(filename, O_RDONLY);
  if (fd < 0)
    return ZX_ERR_IO;
  zx_handle_t vmo;
  zx_handle_t exec_vmo;
  zx_status_t status = fdio_get_vmo_clone(fd, &vmo);
  close(fd);

  if (status != ZX_OK) {
    return status;
  }

  status = zx_vmo_replace_as_executable(vmo, ZX_HANDLE_INVALID, &exec_vmo);
  if (status != ZX_OK) {
    return status;
  }

  if (strlen(filename) >= ZX_MAX_NAME_LEN) {
    const char* p = strrchr(filename, '/');
    if (p != NULL) {
      filename = p + 1;
    }
  }

  status = zx_object_set_property(exec_vmo, ZX_PROP_NAME, filename, strlen(filename));
  if (status != ZX_OK) {
    zx_handle_close(exec_vmo);
    return status;
  }

  *out = exec_vmo;
  return ZX_OK;
}

TEST(DlfcnTests, dlopen_vmo_test) {
  zx_handle_t vmo = ZX_HANDLE_INVALID;
  zx_status_t status = load_vmo(LIBPREFIX "libtrace-engine.so", &vmo);
  EXPECT_EQ(status, ZX_OK, "");
  EXPECT_NE(vmo, ZX_HANDLE_INVALID, "load_vmo");

  void* obj = dlopen_vmo(vmo, RTLD_LOCAL);
  EXPECT_NOT_NULL(obj, "dlopen_vmo");

  zx_handle_close(vmo);

  void* sym = dlsym(obj, "trace_engine_initialize");
  EXPECT_NOT_NULL(sym, "dlsym");

  int ok = dlclose(obj);
  EXPECT_EQ(ok, 0, "dlclose");
}

// This should be some library that this program links against.
#define TEST_SONAME "libfdio.so"
#define TEST_NAME "foobar"
#define TEST_ACTUAL_NAME LIBPREFIX TEST_SONAME

class TestLoaderService : public loader::LoaderServiceBase {
 public:
  static std::shared_ptr<TestLoaderService> Create(async_dispatcher_t* dispatcher) {
    // Can't use make_shared because constructor is private
    return std::shared_ptr<TestLoaderService>(new TestLoaderService(dispatcher));
  }

  int load_object_calls() { return load_object_calls_; }
  int load_object_success() { return load_object_success_; }

 private:
  TestLoaderService(async_dispatcher_t* dispatcher) : LoaderServiceBase(dispatcher, "dlfcn_test") {}

  virtual zx::result<zx::vmo> LoadObjectImpl(std::string name) override {
    ++load_object_calls_;

    if (name != TEST_NAME) {
      printf("loader saw \"%s\", expected \"%s\"", name.c_str(), TEST_NAME);
      return zx::error(ZX_ERR_NOT_FOUND);
    }

    zx::vmo vmo;
    zx_status_t status = load_vmo(TEST_ACTUAL_NAME, vmo.reset_and_get_address());
    if (status != ZX_OK) {
      return zx::error(status);
    }
    ++load_object_success_;
    return zx::ok(std::move(vmo));
  }

  int load_object_calls_ = 0;
  int load_object_success_ = 0;
};

static void show_dlerror(void) { printf("dlerror: %s\n", dlerror()); }

TEST(DlfcnTests, loader_service_test) {
  // Get a handle to an existing library with a known SONAME.
  void* by_name = dlopen(TEST_SONAME, RTLD_NOLOAD);
  EXPECT_NOT_NULL(by_name, "dlopen failed on " TEST_SONAME);
  if (by_name == NULL)
    show_dlerror();

  // Spin up our test service.
  async::Loop loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  ASSERT_OK(loop.StartThread());
  auto loader = TestLoaderService::Create(loop.dispatcher());
  auto loader_conn = loader->Connect();
  ASSERT_OK(loader_conn.status_value());

  // Install the service.
  zx_handle_t old = dl_set_loader_service(loader_conn->TakeChannel().release());
  EXPECT_NE(old, ZX_HANDLE_INVALID, "dl_set_loader_service");

  // Now to a lookup that should go through our service.  It
  // should load up the new copy of the file, find that its
  // SONAME matches an existing library, and just return it.
  void* via_service = dlopen(TEST_NAME, RTLD_LOCAL);

  EXPECT_NOT_NULL(via_service, "dlopen via service");
  if (via_service == NULL) {
    show_dlerror();
  }
  EXPECT_EQ(loader->load_object_calls(), 1, "loader-service not called exactly once");
  EXPECT_EQ(loader->load_object_success(), 1, "loader service call didn't succeed");

  // It should not just have succeeded, but gotten the very
  // same handle as the by-name lookup.
  EXPECT_TRUE(via_service == by_name, "dlopen via service");

  int fail = dlclose(by_name);
  EXPECT_EQ(fail, 0, "dlclose on by-name");
  if (fail)
    show_dlerror();

  fail = dlclose(via_service);
  EXPECT_EQ(fail, 0, "dlclose on via-service");
  if (fail)
    show_dlerror();

  // Put things back to how they were.
  zx_handle_t old2 = dl_set_loader_service(old);
  zx_handle_close(old2);
}

TEST(DlfcnTests, clone_test) {
  zx_handle_t h = ZX_HANDLE_INVALID;
  zx_status_t s = dl_clone_loader_service(&h);
  EXPECT_EQ(s, ZX_OK, "%s", zx_status_get_string(s));
  EXPECT_NE(h, ZX_HANDLE_INVALID, "invalid handle from loader service");

  zx_handle_close(h);
}

void test_global_function(void) {}

TEST(DlfcnTests, dladdr_unexported_test) {
  Dl_info info;
  ASSERT_NE(dladdr((void*)&test_global_function, &info), 0, "dladdr failed");

  // This symbol is not exported to .dynsym, so it won't be found.
  EXPECT_NULL(info.dli_sname, "unexpected symbol name");
  EXPECT_NULL(info.dli_saddr, "unexpected symbol address");
}

// TODO(dbort): Test that this process uses the system loader service by default

TEST(DlfcnTests, dso_no_note_test) {
  void* obj = dlopen("test-dso-no-note.so", RTLD_LOCAL);
  ASSERT_NOT_NULL(obj, "%s", dlerror());

  void* sym = dlsym(obj, "dummy");
  EXPECT_NOT_NULL(sym, "%s", dlerror());

  (*(void (*)(void))(uintptr_t)(sym))();

  EXPECT_EQ(dlclose(obj), 0, "%s", dlerror());
}

#if defined(__aarch64__)
constexpr uint16_t kNativeElfMachine = EM_AARCH64;
#elif defined(__x86_64__)
constexpr uint16_t kNativeElfMachine = EM_X86_64;
#elif defined(__riscv)
constexpr uint16_t kNativeElfMachine = 243;
#else
#error "Unsupported architecture"
#endif

// Matches sizeof(buf) in map_library (zircon/third_party/ulib/musl/ldso/dynlink.c).
constexpr size_t kInitialReadBufSize = sizeof(Elf64_Ehdr) + 16 * sizeof(Elf64_Phdr);

Elf64_Ehdr MakeValidEhdr() {
  Elf64_Ehdr ehdr = {};
  ehdr.e_ident[EI_MAG0] = ELFMAG0;
  ehdr.e_ident[EI_MAG1] = ELFMAG1;
  ehdr.e_ident[EI_MAG2] = ELFMAG2;
  ehdr.e_ident[EI_MAG3] = ELFMAG3;
  ehdr.e_ident[EI_CLASS] = ELFCLASS64;
  ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  ehdr.e_type = ET_DYN;
  ehdr.e_machine = kNativeElfMachine;
  ehdr.e_version = EV_CURRENT;
  ehdr.e_ehsize = sizeof(Elf64_Ehdr);
  ehdr.e_phentsize = sizeof(Elf64_Phdr);
  return ehdr;
}

zx_status_t CreateExecutableElfVmo(const Elf64_Ehdr& ehdr, size_t vmo_size, zx::vmo* out) {
  zx::vmo vmo;
  zx_status_t status = zx::vmo::create(vmo_size, 0, &vmo);
  if (status != ZX_OK) {
    return status;
  }
  if (vmo_size >= sizeof(ehdr)) {
    status = vmo.write(&ehdr, 0, sizeof(ehdr));
    if (status != ZX_OK) {
      return status;
    }
  }
  return vmo.replace_as_executable(zx::resource(), out);
}

TEST(DlfcnTests, dlopen_vmo_relocated_phdrs_test) {
  zx::vmo orig_vmo;
  zx_status_t status =
      load_vmo(LIBPREFIX "test-dso-relocated-phdr.so", orig_vmo.reset_and_get_address());
  if (status != ZX_OK) {
    status = load_vmo("/boot/lib/test-dso-relocated-phdr.so", orig_vmo.reset_and_get_address());
  }
  ASSERT_OK(status);

  uint64_t orig_size = 0;
  ASSERT_OK(orig_vmo.get_size(&orig_size));
  ASSERT_GT(orig_size, kInitialReadBufSize);
  ASSERT_EQ(orig_size % alignof(Elf64_Phdr), 0u);

  Elf64_Ehdr ehdr = {};
  ASSERT_OK(orig_vmo.read(&ehdr, 0, sizeof(ehdr)));
  ASSERT_EQ(ehdr.e_ehsize, sizeof(Elf64_Ehdr));
  ASSERT_EQ(ehdr.e_phentsize, sizeof(Elf64_Phdr));
  ASSERT_GT(ehdr.e_phnum, 0);
  ASSERT_LE(ehdr.e_phnum, 16);

  const size_t phsize = static_cast<size_t>(ehdr.e_phentsize) * ehdr.e_phnum;
  ASSERT_LE(ehdr.e_phoff, orig_size);
  ASSERT_LE(phsize, orig_size - ehdr.e_phoff);

  std::vector<Elf64_Phdr> phdrs(ehdr.e_phnum);
  ASSERT_OK(orig_vmo.read(phdrs.data(), ehdr.e_phoff, phsize));
  for (Elf64_Phdr& phdr : phdrs) {
    if (phdr.p_type == PT_PHDR) {
      phdr.p_type = PT_NULL;
    }
  }

  std::vector<uint8_t> image(orig_size);
  ASSERT_OK(orig_vmo.read(image.data(), 0, orig_size));
  if (ehdr.e_phoff >= sizeof(Elf64_Ehdr)) {
    memset(image.data() + ehdr.e_phoff, 0, phsize);
  }

  const uint64_t new_phoff = orig_size;
  ehdr.e_phoff = new_phoff;
  memcpy(image.data(), &ehdr, sizeof(ehdr));

  zx::vmo relocated_vmo;
  ASSERT_OK(zx::vmo::create(orig_size + phsize, 0, &relocated_vmo));
  uint64_t relocated_vmo_size = 0;
  ASSERT_OK(relocated_vmo.get_size(&relocated_vmo_size));
  ASSERT_LE(new_phoff + phsize, relocated_vmo_size);

  ASSERT_OK(relocated_vmo.write(image.data(), 0, image.size()));
  ASSERT_OK(relocated_vmo.write(phdrs.data(), new_phoff, phsize));
  ASSERT_OK(relocated_vmo.replace_as_executable(zx::resource(), &relocated_vmo));

  void* obj = dlopen_vmo(relocated_vmo.get(), RTLD_LOCAL);
  ASSERT_NOT_NULL(obj, "%s", dlerror());

  void* sym = dlsym(obj, "dummy");
  ASSERT_NOT_NULL(sym, "%s", dlerror());

  reinterpret_cast<void (*)()>(reinterpret_cast<uintptr_t>(sym))();

  EXPECT_EQ(dlclose(obj), 0, "%s", dlerror());
}

TEST(DlfcnTests, dlopen_vmo_malformed_phoff_test) {
  const size_t page_size = zx_system_get_page_size();

  // 0-byte VMO (smaller than sizeof(Elf64_Ehdr)).
  {
    zx::vmo empty_vmo;
    ASSERT_OK(zx::vmo::create(0, 0, &empty_vmo));
    ASSERT_OK(empty_vmo.replace_as_executable(zx::resource(), &empty_vmo));
    EXPECT_NULL(dlopen_vmo(empty_vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }

  // e_phoff exceeds the VMO size.
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phnum = 1;
    ehdr.e_phoff = page_size + sizeof(Elf64_Phdr);
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }

  // e_phoff <= VMO size, but e_phoff + phsize exceeds the VMO size.
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phnum = 2;
    ehdr.e_phoff = page_size - sizeof(Elf64_Phdr);
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }

  // e_phoff + phsize overflows size_t and wraps into [0, kInitialReadBufSize].
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phnum = 2;
    ehdr.e_phoff = std::numeric_limits<size_t>::max() - sizeof(Elf64_Phdr) + 1;
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }

  // e_phoff + phsize overflows size_t to 0.
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phnum = 1;
    ehdr.e_phoff = std::numeric_limits<size_t>::max() - sizeof(Elf64_Phdr) + 1;
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }

  // e_phoff < kInitialReadBufSize, but e_phoff + phsize > kInitialReadBufSize.
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phnum = 2;
    ehdr.e_phoff = sizeof(Elf64_Ehdr) + 15 * sizeof(Elf64_Phdr) + alignof(Elf64_Phdr);
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }

  // Invalid e_phentsize (both smaller than Elf64_Phdr and overflowing int32_t product).
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phentsize = 1;
    ehdr.e_phnum = 1;
    ehdr.e_phoff = kInitialReadBufSize - 1;
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }
  {
    Elf64_Ehdr ehdr = MakeValidEhdr();
    ehdr.e_phentsize = std::numeric_limits<uint16_t>::max();
    ehdr.e_phnum = std::numeric_limits<uint16_t>::max();
    ehdr.e_phoff = sizeof(Elf64_Ehdr);
    zx::vmo vmo;
    ASSERT_OK(CreateExecutableElfVmo(ehdr, page_size, &vmo));
    EXPECT_NULL(dlopen_vmo(vmo.get(), RTLD_LOCAL));
    EXPECT_NOT_NULL(dlerror());
  }
}

}  // namespace
