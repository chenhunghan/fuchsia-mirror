// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/magma/platform/platform_mmio.h>
#include <lib/magma_service/mock/mock_bus_mapper.h>
#include <lib/magma_service/mock/mock_mmio.h>

#include <future>

#include <gtest/gtest.h>

#include "driver_logger_harness.h"
#include "src/graphics/drivers/msd-arm-mali/src/address_manager.h"
#include "src/graphics/drivers/msd-arm-mali/src/registers.h"
#include "src/graphics/drivers/msd-arm-mali/tests/unit_tests/fake_connection_owner_base.h"

namespace {

class FakeOwner : public AddressManager::Owner {
 public:
  FakeOwner(mali::RegisterIo* regs) : register_io_(regs) {}

  mali::RegisterIo* register_io() override { return register_io_; }

 private:
  mali::RegisterIo* register_io_;
};

class TestConnectionOwner : public FakeConnectionOwnerBase {
 public:
  TestConnectionOwner(AddressManager* manager) : manager_(manager) {}

  void NdtPostScheduleAtom(std::shared_ptr<MsdArmAtom> atom) override {}
  void NdtPostCancelAtoms(std::shared_ptr<MsdArmConnection> connection) override {}
  AddressSpaceObserver* NdtGetAddressSpaceObserver() override { return manager_; }
  magma::PlatformBusMapper* NdtGetBusMapper() override { return &bus_mapper_; }

 private:
  AddressManager* manager_;
  MockBusMapper bus_mapper_;
};

static constexpr uint64_t kMemoryAttributes = 0x8848u;

class AddressManagerTest : public testing::Test {
  void SetUp() override { logger_harness_ = DriverLoggerHarness::Create(); }
  std::unique_ptr<DriverLoggerHarness> logger_harness_;
};

TEST_F(AddressManagerTest, MultipleAtoms) {
  auto reg_io = std::make_unique<mali::RegisterIo>(MockMmio::Create(1024 * 1024));
  FakeOwner owner(reg_io.get());
  AddressManager address_manager(&owner, 8);
  TestConnectionOwner connection_owner(&address_manager);
  std::shared_ptr<MsdArmConnection> connection1 = MsdArmConnection::Create(0, &connection_owner);
  auto atom1 = std::make_unique<MsdArmAtom>(connection1, 0, 0, 0, magma_arm_mali_user_data(), 0);

  EXPECT_TRUE(address_manager.AssignAddressSpace(atom1.get()));

  std::shared_ptr<MsdArmConnection> connection2 = MsdArmConnection::Create(0, &connection_owner);
  auto atom2 = std::make_unique<MsdArmAtom>(connection2, 0, 0, 0, magma_arm_mali_user_data(), 0);
  EXPECT_TRUE(address_manager.AssignAddressSpace(atom2.get()));

  EXPECT_EQ(0u, atom1->address_slot_mapping()->slot_number());
  EXPECT_EQ(1u, atom2->address_slot_mapping()->slot_number());

  registers::AsRegisters as_regs(0);
  EXPECT_EQ(kMemoryAttributes, as_regs.MemoryAttributes().ReadFrom(reg_io.get()).reg_value());
  uint64_t translation_table_entry1 = connection1->const_address_space()->translation_table_entry();
  EXPECT_EQ(translation_table_entry1,
            as_regs.TranslationTable().ReadFrom(reg_io.get()).reg_value());

  registers::AsRegisters as_regs1(1);
  EXPECT_EQ(kMemoryAttributes, as_regs1.MemoryAttributes().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(connection2->const_address_space()->translation_table_entry(),
            as_regs1.TranslationTable().ReadFrom(reg_io.get()).reg_value());

  connection1.reset();
  // atom1 should hold a reference to the translation table entry.
  EXPECT_EQ(translation_table_entry1,
            as_regs.TranslationTable().ReadFrom(reg_io.get()).reg_value());

  address_manager.AtomFinished(atom1.get());
  EXPECT_EQ(kMemoryAttributes, as_regs.MemoryAttributes().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(0u, as_regs.TranslationTable().ReadFrom(reg_io.get()).reg_value() & 0xff);

  EXPECT_FALSE(address_manager.AssignAddressSpace(atom1.get()));

  address_manager.AtomFinished(atom2.get());

  auto atom3 = std::make_unique<MsdArmAtom>(connection2, 0, 0, 0, magma_arm_mali_user_data(), 0);
  EXPECT_TRUE(address_manager.AssignAddressSpace(atom3.get()));
  EXPECT_EQ(1u, atom3->address_slot_mapping()->slot_number());
}

TEST_F(AddressManagerTest, PreferUnused) {
  auto reg_io = std::make_unique<mali::RegisterIo>(MockMmio::Create(1024 * 1024));
  FakeOwner owner(reg_io.get());
  AddressManager address_manager(&owner, 8);
  TestConnectionOwner connection_owner(&address_manager);
  std::shared_ptr<MsdArmConnection> connection1 = MsdArmConnection::Create(0, &connection_owner);
  auto atom1 = std::make_unique<MsdArmAtom>(connection1, 0, 0, 0, magma_arm_mali_user_data(), 0);

  EXPECT_TRUE(address_manager.AssignAddressSpace(atom1.get()));
  EXPECT_EQ(0u, atom1->address_slot_mapping()->slot_number());
  address_manager.AtomFinished(atom1.get());

  std::shared_ptr<MsdArmConnection> connection2 = MsdArmConnection::Create(0, &connection_owner);
  auto atom2 = std::make_unique<MsdArmAtom>(connection2, 0, 0, 0, magma_arm_mali_user_data(), 0);
  EXPECT_TRUE(address_manager.AssignAddressSpace(atom2.get()));

  // Slots that are mapped to connections should only be reused if empty
  // slots are not available.
  EXPECT_EQ(1u, atom2->address_slot_mapping()->slot_number());
}

TEST_F(AddressManagerTest, ReuseSlot) {
  auto reg_io = std::make_unique<mali::RegisterIo>(MockMmio::Create(1024 * 1024));
  FakeOwner owner(reg_io.get());

  const uint32_t kNumberAddressSpaces = 8;
  AddressManager address_manager(&owner, kNumberAddressSpaces);
  TestConnectionOwner connection_owner(&address_manager);

  std::vector<std::shared_ptr<MsdArmConnection>> connections;
  std::vector<std::unique_ptr<MsdArmAtom>> atoms;
  for (size_t i = 0; i < kNumberAddressSpaces; i++) {
    connections.push_back(MsdArmConnection::Create(0, &connection_owner));
    atoms.push_back(
        std::make_unique<MsdArmAtom>(connections.back(), 0, 0, 0, magma_arm_mali_user_data(), 0));
    EXPECT_TRUE(address_manager.AssignAddressSpace(atoms.back().get()));
  }

  registers::AsRegisters as_regs(2);
  EXPECT_EQ(kMemoryAttributes, as_regs.MemoryAttributes().ReadFrom(reg_io.get()).reg_value());
  uint64_t translation_table_entry =
      connections[2]->const_address_space()->translation_table_entry();
  EXPECT_EQ(translation_table_entry, as_regs.TranslationTable().ReadFrom(reg_io.get()).reg_value());

  connections.push_back(MsdArmConnection::Create(0, &connection_owner));
  atoms.push_back(
      std::make_unique<MsdArmAtom>(connections.back(), 0, 0, 0, magma_arm_mali_user_data(), 0));
  // Reduce timeout to make test faster.
  address_manager.set_acquire_slot_timeout_seconds(1);
  EXPECT_FALSE(address_manager.AssignAddressSpace(atoms.back().get()));
  address_manager.set_acquire_slot_timeout_seconds(10);
  address_manager.set_increase_notify_race_window(true);

  auto future = std::async(std::launch::async, [&]() {
    // Sleep to try to ensure AssignAddressSpace is currently running.
    usleep(10000);
    address_manager.AtomFinished(atoms[2].get());
  });

  EXPECT_TRUE(address_manager.AssignAddressSpace(atoms.back().get()));

  uint64_t new_translation_table_entry =
      connections[8]->const_address_space()->translation_table_entry();
  EXPECT_EQ(new_translation_table_entry,
            as_regs.TranslationTable().ReadFrom(reg_io.get()).reg_value());
}

TEST_F(AddressManagerTest, FlushAddressRange) {
  auto reg_io = std::make_unique<mali::RegisterIo>(MockMmio::Create(1024 * 1024));
  FakeOwner owner(reg_io.get());
  auto mapper = std::unique_ptr<MockBusMapper>();

  const uint32_t kNumberAddressSpaces = 8;
  AddressManager address_manager(&owner, kNumberAddressSpaces);
  TestConnectionOwner connection_owner(&address_manager);
  std::shared_ptr<MsdArmConnection> connection = MsdArmConnection::Create(0, &connection_owner);

  auto atom = std::make_unique<MsdArmAtom>(connection, 0, 0, 0, magma_arm_mali_user_data(), 0);
  EXPECT_TRUE(address_manager.AssignAddressSpace(atom.get()));

  const size_t page_size = zx_system_get_page_size();
  uint64_t addr = page_size * 0xbdefcccec;

  // Keep dummy mapped to prevent GC of the page table.
  uint64_t dummy_addr = page_size * 0xbdefccc00;
  auto dummy_buffer = magma::PlatformBuffer::Create(page_size, "dummy");
  auto dummy_bus_mapping =
      connection_owner.NdtGetBusMapper()->MapPageRangeBus(dummy_buffer.get(), 0, 1);
  EXPECT_TRUE(connection->address_space_for_testing()->Insert(dummy_addr, dummy_bus_mapping.get(),
                                                              0, page_size, kAccessFlagRead));

  std::unique_ptr<magma::PlatformBuffer> buffer;

  buffer = magma::PlatformBuffer::Create(page_size * 3, "test");

  auto bus_mapping = connection_owner.NdtGetBusMapper()->MapPageRangeBus(
      buffer.get(), 0, buffer->size() / page_size);
  ASSERT_NE(nullptr, bus_mapping);

  EXPECT_TRUE(connection->address_space_for_testing()->Insert(
      addr, bus_mapping.get(), 0, buffer->size(), kAccessFlagRead | kAccessFlagNoExecute));
  // 3 pages should be cleared. Since the address is aligned to 4 pages,
  // it should be rounded up to 4 (log base 2 is 2). Width is 2 + 11 = 13.
  constexpr uint64_t kLockOffset = 13;
  registers::AsRegisters as_regs(0);
  EXPECT_EQ(addr | kLockOffset, as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdFlushPageTable,
            as_regs.Command().ReadFrom(reg_io.get()).reg_value());

  EXPECT_TRUE(connection->address_space_for_testing()->Clear(addr, buffer->size()));

  EXPECT_EQ(addr | kLockOffset, as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdFlushMem,
            as_regs.Command().ReadFrom(reg_io.get()).reg_value());
  address_manager.AtomFinished(atom.get());
  connection.reset();

  // Clear entire address range.
  EXPECT_EQ(10u + (48 - kMaliPageShift) + 1,
            as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdUpdate, as_regs.Command().ReadFrom(reg_io.get()).reg_value());
}

TEST_F(AddressManagerTest, FlushAddressRangeMisaligned) {
  auto reg_io = std::make_unique<mali::RegisterIo>(MockMmio::Create(1024 * 1024));
  FakeOwner owner(reg_io.get());
  auto mapper = std::unique_ptr<MockBusMapper>();

  const uint32_t kNumberAddressSpaces = 8;
  AddressManager address_manager(&owner, kNumberAddressSpaces);
  TestConnectionOwner connection_owner(&address_manager);
  std::shared_ptr<MsdArmConnection> connection = MsdArmConnection::Create(0, &connection_owner);

  auto atom = std::make_unique<MsdArmAtom>(connection, 0, 0, 0, magma_arm_mali_user_data(), 0);
  EXPECT_TRUE(address_manager.AssignAddressSpace(atom.get()));

  const size_t page_size = zx_system_get_page_size();
  uint64_t addr = page_size * 0xbdefcccef;

  // Keep dummy mapped to prevent GC of the page table.
  uint64_t dummy_addr = page_size * 0xbdefccc00;
  auto dummy_buffer = magma::PlatformBuffer::Create(page_size, "dummy");
  auto dummy_bus_mapping =
      connection_owner.NdtGetBusMapper()->MapPageRangeBus(dummy_buffer.get(), 0, 1);
  EXPECT_TRUE(connection->address_space_for_testing()->Insert(dummy_addr, dummy_bus_mapping.get(),
                                                              0, page_size, kAccessFlagRead));

  std::unique_ptr<magma::PlatformBuffer> buffer;

  buffer = magma::PlatformBuffer::Create(page_size * 3, "test");

  auto bus_mapping = connection_owner.NdtGetBusMapper()->MapPageRangeBus(
      buffer.get(), 0, buffer->size() / page_size);
  ASSERT_NE(nullptr, bus_mapping);

  EXPECT_TRUE(connection->address_space_for_testing()->Insert(
      addr, bus_mapping.get(), 0, buffer->size(), kAccessFlagRead | kAccessFlagNoExecute));
  // 3 pages should be cleared. Since the address is not aligned to 4 pages,
  // it crosses a boundary and needs to be rounded up to 32 pages to cover the range.
  // log base 2 of 32 is 5. Width is 5 + 11 = 16.
  constexpr uint64_t kLockOffset = 16;
  registers::AsRegisters as_regs(0);
  EXPECT_EQ(addr | kLockOffset, as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdFlushPageTable,
            as_regs.Command().ReadFrom(reg_io.get()).reg_value());

  EXPECT_TRUE(connection->address_space_for_testing()->Clear(addr, buffer->size()));

  EXPECT_EQ(addr | kLockOffset, as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdFlushMem,
            as_regs.Command().ReadFrom(reg_io.get()).reg_value());
  address_manager.AtomFinished(atom.get());
  connection.reset();

  // Clear entire address range.
  EXPECT_EQ(10u + (48 - kMaliPageShift) + 1,
            as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdUpdate, as_regs.Command().ReadFrom(reg_io.get()).reg_value());
}

TEST_F(AddressManagerTest, FlushAddressRangeReaped) {
  auto reg_io = std::make_unique<mali::RegisterIo>(MockMmio::Create(1024 * 1024));
  FakeOwner owner(reg_io.get());
  auto mapper = std::unique_ptr<MockBusMapper>();

  const uint32_t kNumberAddressSpaces = 8;
  AddressManager address_manager(&owner, kNumberAddressSpaces);
  TestConnectionOwner connection_owner(&address_manager);
  std::shared_ptr<MsdArmConnection> connection = MsdArmConnection::Create(0, &connection_owner);

  auto atom = std::make_unique<MsdArmAtom>(connection, 0, 0, 0, magma_arm_mali_user_data(), 0);
  EXPECT_TRUE(address_manager.AssignAddressSpace(atom.get()));

  const size_t page_size = zx_system_get_page_size();
  uint64_t addr = page_size * 0xbdefcccec;

  // Keep dummy mapped in the next level 0 page table to prevent GC of the level 1 page table.
  uint64_t dummy_addr = page_size * 0xbdefcce00;
  auto dummy_buffer = magma::PlatformBuffer::Create(page_size, "dummy");
  auto dummy_bus_mapping =
      connection_owner.NdtGetBusMapper()->MapPageRangeBus(dummy_buffer.get(), 0, 1);
  EXPECT_TRUE(connection->address_space_for_testing()->Insert(dummy_addr, dummy_bus_mapping.get(),
                                                              0, page_size, kAccessFlagRead));

  std::unique_ptr<magma::PlatformBuffer> buffer;

  buffer = magma::PlatformBuffer::Create(page_size * 3, "test");

  auto bus_mapping = connection_owner.NdtGetBusMapper()->MapPageRangeBus(
      buffer.get(), 0, buffer->size() / page_size);
  ASSERT_NE(nullptr, bus_mapping);

  EXPECT_TRUE(connection->address_space_for_testing()->Insert(
      addr, bus_mapping.get(), 0, buffer->size(), kAccessFlagRead | kAccessFlagNoExecute));

  // The insert should have flushed the range [addr, 3 pages).
  // 3 pages aligned to 4 -> width 13.
  constexpr uint64_t kLockOffset = 13;
  registers::AsRegisters as_regs(0);
  EXPECT_EQ(addr | kLockOffset, as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdFlushPageTable,
            as_regs.Command().ReadFrom(reg_io.get()).reg_value());

  // Now clear. Since there is no other mapping in the level 0 page table,
  // it should be reaped.
  // The reaped table covers [0xbdefccc00 * page_size, 2MB).
  // So the flush range should be widened to cover it.
  // Start: 0xbdefccc00 * page_size = 0xbdefccc00000.
  // Size: 2MB (512 pages).
  // Width: log2(512) + 11 = 9 + 11 = 20.
  // Expected LockAddress: 0xbdefccc00000 | 20 = 0xbdefccc00014.
  EXPECT_TRUE(connection->address_space_for_testing()->Clear(addr, buffer->size()));

  uint64_t expected_reaped_addr = page_size * 0xbdefccc00;
  constexpr uint64_t kReapedLockOffset = 20;  // log2(512) + 11
  EXPECT_EQ(expected_reaped_addr | kReapedLockOffset,
            as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdFlushMem,
            as_regs.Command().ReadFrom(reg_io.get()).reg_value());

  address_manager.AtomFinished(atom.get());
  connection.reset();

  // Clear entire address range on connection destruction.
  EXPECT_EQ(10u + (48 - kMaliPageShift) + 1,
            as_regs.LockAddress().ReadFrom(reg_io.get()).reg_value());
  EXPECT_EQ(registers::AsCommand::kCmdUpdate, as_regs.Command().ReadFrom(reg_io.get()).reg_value());
}

TEST(AddressManager, GetLog2FlushRegionPages) {
  constexpr uint64_t kPageSize = 4096;

  // 0 pages
  EXPECT_EQ(0u, GetLog2FlushRegionPages(0, 0));
  EXPECT_EQ(0u, GetLog2FlushRegionPages(kPageSize, kPageSize));

  // 1 page
  EXPECT_EQ(0u, GetLog2FlushRegionPages(0, kPageSize));
  EXPECT_EQ(0u, GetLog2FlushRegionPages(kPageSize, kPageSize * 2));
  EXPECT_EQ(0u, GetLog2FlushRegionPages(kPageSize * 100, kPageSize * 101));

  // 2 pages
  // Aligned
  EXPECT_EQ(1u, GetLog2FlushRegionPages(0, kPageSize * 2));
  EXPECT_EQ(1u, GetLog2FlushRegionPages(kPageSize * 2, kPageSize * 4));
  // Misaligned
  EXPECT_EQ(2u, GetLog2FlushRegionPages(kPageSize, kPageSize * 3));
  EXPECT_EQ(3u, GetLog2FlushRegionPages(kPageSize * 3, kPageSize * 5));
  EXPECT_EQ(2u, GetLog2FlushRegionPages(kPageSize * 5, kPageSize * 7));

  // 3 pages
  EXPECT_EQ(2u, GetLog2FlushRegionPages(0, kPageSize * 3));
  EXPECT_EQ(2u, GetLog2FlushRegionPages(kPageSize, kPageSize * 4));
  EXPECT_EQ(3u, GetLog2FlushRegionPages(kPageSize * 2, kPageSize * 5));

  // 4 pages
  // Aligned
  EXPECT_EQ(2u, GetLog2FlushRegionPages(0, kPageSize * 4));
  EXPECT_EQ(2u, GetLog2FlushRegionPages(kPageSize * 4, kPageSize * 8));
  // Misaligned
  EXPECT_EQ(3u, GetLog2FlushRegionPages(kPageSize, kPageSize * 5));
  EXPECT_EQ(3u, GetLog2FlushRegionPages(kPageSize * 2, kPageSize * 6));
  EXPECT_EQ(3u, GetLog2FlushRegionPages(kPageSize * 3, kPageSize * 7));

  // Large ranges
  EXPECT_EQ(5u, GetLog2FlushRegionPages(0, kPageSize * 32));
  EXPECT_EQ(6u, GetLog2FlushRegionPages(kPageSize, kPageSize * 33));
}

}  // namespace
