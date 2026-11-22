// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_MOCK_DEVICE_UICCMD_PROCESSOR_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_MOCK_DEVICE_UICCMD_PROCESSOR_H_

#include <cstdint>
#include <functional>
#include <initializer_list>
#include <map>
#include <mutex>
#include <optional>
#include <set>
#include <vector>

#include "handler.h"
#include "src/devices/block/drivers/ufs/ufs.h"

namespace ufs {
namespace ufs_mock_device {

class UfsMockDevice;

// Identifies a UniPro DME attribute in the mock MIB table: attribute ID, local (DME_GET/SET) vs
// peer (DME_PEER_GET/SET) side, and GenSelectorIndex.
struct MibKey {
  uint16_t attribute = 0;
  bool peer = false;
  uint16_t selector = 0;

  auto operator<=>(const MibKey &) const = default;
};

struct DmeSetRecord {
  MibKey key;
  uint32_t value = 0;

  auto operator<=>(const DmeSetRecord &) const = default;
};

class UicCmdProcessor {
 public:
  using UicCmdHandler = std::function<void(UfsMockDevice &, uint32_t, uint32_t, uint32_t)>;
  using MibKey = ufs_mock_device::MibKey;
  using DmeSetRecord = ufs_mock_device::DmeSetRecord;

  UicCmdProcessor(const UicCmdProcessor &) = delete;
  UicCmdProcessor &operator=(const UicCmdProcessor &) = delete;
  UicCmdProcessor(const UicCmdProcessor &&) = delete;
  UicCmdProcessor &operator=(const UicCmdProcessor &&) = delete;
  ~UicCmdProcessor() = default;
  explicit UicCmdProcessor(UfsMockDevice &mock_device) : mock_device_(mock_device) {}

  void HandleUicCmd(UicCommandOpcode value);

  // Seeds an attribute in the fake MIB table. Seeded attributes behave like storage: DME_(PEER_)GET
  // returns the stored value and DME_(PEER_)SET updates it in place. Unseeded attributes retain the
  // mock's compile-time defaults and discard writes so existing tests are unaffected.
  void SetMibValue(uint16_t attribute, uint32_t value, bool peer = false, uint16_t selector = 0);
  std::optional<uint32_t> GetMibValue(uint16_t attribute, bool peer = false,
                                      uint16_t selector = 0) const;

  // Returns successful DME_SET and DME_PEER_SET operations in issue order, optionally filtered to
  // `attributes`.
  std::vector<DmeSetRecord> GetDmeSetLog(std::initializer_list<uint16_t> attributes = {}) const;
  // Returns the values written to one attribute in issue order.
  std::vector<uint32_t> GetDmeSets(uint16_t attribute, bool peer = false,
                                   uint16_t selector = 0) const;
  // Returns the number of DME_GET or DME_PEER_GET reads of one attribute.
  uint32_t GetDmeGetCount(uint16_t attribute, bool peer = false, uint16_t selector = 0) const;

  // Forces subsequent DME_SET (or DME_PEER_SET when `peer` is true) commands for `attribute` to
  // complete with UICCMDARG2.result_code = kFailure without updating the MIB table or set log.
  void FailDmeSet(uint16_t attribute, bool peer = false, uint16_t selector = 0);

  // Invoked by Handler::Reset() via DoResetExtra() to clear seeded MIB state, logs, and failures.
  void ResetExtra();

  static void DefaultDmeGetHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2,
                                   uint32_t ucmdarg3);
  static void DefaultDmeSetHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2,
                                   uint32_t ucmdarg3);
  static void DefaultDmePeerGetHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1,
                                       uint32_t ucmdarg2, uint32_t ucmdarg3);
  static void DefaultDmePeerSetHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1,
                                       uint32_t ucmdarg2, uint32_t ucmdarg3);
  static void DefaultDmeLinkStartUpHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1,
                                           uint32_t ucmdarg2, uint32_t ucmdarg3);
  static void DefaultDmeHibernateEnterHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1,
                                              uint32_t ucmdarg2, uint32_t ucmdarg3);
  static void DefaultDmeHibernateExitHandler(UfsMockDevice &mock_device, uint32_t ucmdarg1,
                                             uint32_t ucmdarg2, uint32_t ucmdarg3);

  DEF_DEFAULT_HANDLER_BEGIN(UicCommandOpcode, UicCmdHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmeGet, DefaultDmeGetHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmeSet, DefaultDmeSetHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmePeerGet, DefaultDmePeerGetHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmePeerSet, DefaultDmePeerSetHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmeLinkStartUp, DefaultDmeLinkStartUpHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmeHibernateEnter, DefaultDmeHibernateEnterHandler)
  DEF_DEFAULT_HANDLER(UicCommandOpcode::kDmeHibernateExit, DefaultDmeHibernateExitHandler)
  DEF_DEFAULT_HANDLER_END()

 private:
  std::optional<uint32_t> RecordGetAndLookup(const MibKey &key);
  bool RecordSetOrFail(const MibKey &key, uint32_t value);

  UfsMockDevice &mock_device_;
  mutable std::mutex lock_;
  std::map<MibKey, uint32_t> mib_;
  std::vector<DmeSetRecord> set_log_;
  std::map<MibKey, uint32_t> get_counts_;
  std::set<MibKey> failing_sets_;
};

}  // namespace ufs_mock_device
}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_MOCK_DEVICE_UICCMD_PROCESSOR_H_
