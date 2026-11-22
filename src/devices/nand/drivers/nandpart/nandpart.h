// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_NAND_DRIVERS_NANDPART_NANDPART_H_
#define SRC_DEVICES_NAND_DRIVERS_NANDPART_NANDPART_H_

#include <fidl/fuchsia.hardware.badblock/cpp/wire.h>
#include <fuchsia/hardware/nand/c/banjo.h>
#include <fuchsia/hardware/nand/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_offers.h>
#include <zircon/types.h>

#include "bad-block.h"

namespace nand {

class NandPartDevice : public ddk::NandProtocol<NandPartDevice>,
                       public fidl::WireServer<fuchsia_hardware_badblock::BadBlock> {
 public:
  explicit NandPartDevice(const nand_protocol_t& nand_proto, std::shared_ptr<BadBlock> bad_block,
                          size_t parent_op_size, const nand_info_t& nand_info,
                          uint32_t erase_block_start, std::string name)
      : nand_proto_(nand_proto),
        nand_(&nand_proto_),
        parent_op_size_(parent_op_size),
        nand_info_(nand_info),
        erase_block_start_(erase_block_start),
        bad_block_(std::move(bad_block)),
        name_(std::move(name)) {}

  ~NandPartDevice() {
    bad_block_bindings_.RemoveAll();
    if (outgoing_) {
      (void)outgoing_->RemoveService<fuchsia_hardware_badblock::Service>(name_);
    }
  }

  zx::result<> Init(uint32_t copy_count,
                    fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
                    const std::optional<std::string>& node_name,
                    const std::shared_ptr<fdf::Namespace>& incoming,
                    const std::shared_ptr<fdf::OutgoingDirectory>& outgoing);

  // nand protocol implementation.
  void NandQuery(nand_info_t* info_out, size_t* nand_op_size_out);
  void NandQueue(nand_operation_t* op, nand_queue_callback completion_cb, void* cookie);
  zx_status_t NandGetFactoryBadBlockList(uint32_t* bad_blocks, size_t bad_block_len,
                                         size_t* num_bad_blocks);

  // Bad block FIDL implementation.
  void GetBadBlockList(GetBadBlockListCompleter::Sync& completer) override;
  void MarkBlockBad(MarkBlockBadRequestView request,
                    MarkBlockBadCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_badblock::BadBlock> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

 private:
  nand_protocol_t nand_proto_;
  ddk::NandProtocolClient nand_;

  // op_size for parent device.
  size_t parent_op_size_;
  // info about nand.
  nand_info_t nand_info_;
  // First erase block for the partition.
  uint32_t erase_block_start_;
  // Device specific bad block info. Shared between all devices for a given
  // parent device.
  std::shared_ptr<BadBlock> bad_block_;
  // Cached list of bad blocks for this partition. Lazily instantiated.
  std::vector<uint32_t> bad_blocks_;
  uint32_t extra_partition_copy_count_;

  const std::string name_;

  fidl::ClientEnd<fuchsia_driver_framework::NodeController> child_;

  compat::BanjoServer nand_server_{ZX_PROTOCOL_NAND, this, &nand_protocol_ops_};
  compat::SyncInitializedDeviceServer compat_server_;
  fidl::ServerBindingGroup<fuchsia_hardware_badblock::BadBlock> bad_block_bindings_;
  std::shared_ptr<fdf::OutgoingDirectory> outgoing_;
};

class Driver : public fdf::DriverBase2 {
 public:
  static constexpr std::string_view kDriverName = "nandpart";

  explicit Driver() : fdf::DriverBase2(kDriverName) {}

  // fdf::DriverBase2 implementation.
  zx::result<> Start(fdf::DriverContext context) override;

 private:
  std::vector<std::unique_ptr<NandPartDevice>> devices_;
};

}  // namespace nand

#endif  // SRC_DEVICES_NAND_DRIVERS_NANDPART_NANDPART_H_
