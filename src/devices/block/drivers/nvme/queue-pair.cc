// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/queue-pair.h"

#include <lib/driver/logging/cpp/logger.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

#include <sdk/lib/driver/logging/cpp/logger.h>

#include "src/devices/block/drivers/nvme/commands.h"
#include "src/devices/block/drivers/nvme/commands/nvme-io.h"
#include "src/devices/block/drivers/nvme/registers.h"

namespace nvme {

// The pinned page addresses of a transaction, including the PRP list, are stored in a single PRP
// buffer page. The smallest supported page size is 4096 bytes.
static_assert(QueuePair::kMaxPinnedPages <= 4096 / sizeof(zx_paddr_t));

zx::result<std::unique_ptr<QueuePair>> QueuePair::Create(zx::unowned_bti bti, uint16_t queue_id,
                                                         uint32_t max_entries, CapabilityReg& caps,
                                                         fdf::MmioBuffer& mmio, bool prealloc_prp) {
  auto completion_queue = Queue::Create(bti->borrow(), queue_id, max_entries, sizeof(Completion));
  if (completion_queue.is_error()) {
    return completion_queue.take_error();
  }

  auto submission_queue = Queue::Create(bti->borrow(), queue_id, max_entries, sizeof(Submission));
  if (submission_queue.is_error()) {
    return submission_queue.take_error();
  }

  fbl::AllocChecker ac;
  fbl::Vector<TransactionData> txns;
  txns.resize(submission_queue->entry_count(), &ac);
  if (!ac.check()) {
    fdf::error("Failed to allocate memory for {} transactions for queue pair with id {}.",
               submission_queue->entry_count(), queue_id);
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  auto completion_doorbell = DoorbellReg::CompletionQueue(queue_id, caps).FromValue(0);
  auto submission_doorbell = DoorbellReg::SubmissionQueue(queue_id, caps).FromValue(0);

  auto queue_pair = fbl::make_unique_checked<QueuePair>(
      &ac, std::move(*completion_queue), std::move(*submission_queue), std::move(txns),
      std::move(bti), mmio, completion_doorbell, submission_doorbell);
  if (!ac.check()) {
    fdf::error("Failed to allocate memory for queue pair with id {}.", queue_id);
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  if (prealloc_prp) {
    zx_status_t status = queue_pair->PreallocatePrpBuffers();
    if (status != ZX_OK) {
      return zx::error(status);
    }
  }
  return zx::ok(std::move(queue_pair));
}

zx_status_t QueuePair::PreallocatePrpBuffers() {
  auto buffer_factory = dma_buffer::CreateBufferFactory();
  for (auto& txn_data : txns_) {
    zx_status_t status =
        buffer_factory->CreateContiguous(*bti_, zx_system_get_page_size(), 0,
                                         dma_buffer::CacheOptions::kEnabled, &txn_data.prp_buffer);
    if (status != ZX_OK) {
      return status;
    }
  }
  return ZX_OK;
}

zx_status_t QueuePair::CheckForNewCompletion(Completion** completion, IoCommand** io_cmd) {
  if (io_cmd != nullptr) {
    *io_cmd = nullptr;
  }
  if (static_cast<Completion*>(completion_.Peek())->phase() != completion_ready_phase_) {
    return ZX_ERR_SHOULD_WAIT;
  }

  *completion = static_cast<Completion*>(completion_.Next());
  if (completion_.NextIndex() == 0) {
    // Toggle the ready phase when we're about to wrap around.
    completion_ready_phase_ ^= 1;
  }
  sq_head_ = (*completion)->sq_head();

  auto txn_id = (*completion)->command_id();
  {
    if (txn_id >= txns_.size()) {
      fdf::error("Completed transaction has invalid ID: {}", txn_id);
      return ZX_ERR_BAD_STATE;
    }
    TransactionData& txn_data = txns_[txn_id];
    if (!txn_data.active) {
      fdf::error("Completed transaction #{} was not active.", txn_id);
      return ZX_ERR_BAD_STATE;
    }

    if (io_cmd != nullptr) {
      *io_cmd = txn_data.io_cmd;
    }
    ReleaseTransaction(txn_data);
  }

  return ZX_OK;
}

void QueuePair::ReleaseTransaction(TransactionData& txn_data) {
  if (txn_data.pmt.is_valid()) {
    zx_status_t status = txn_data.pmt.unpin();
    if (status != ZX_OK) {
      // This does not affect the outcome of the transfer, so it must not prevent the transaction
      // from being completed or its command ID from being reused.
      fdf::error("Failed to unpin IO buffer: {}", zx_status_get_string(status));
    }
  }
  txn_data.ClearExceptPrp();
  --in_flight_count_;
}

void QueuePair::CancelAll(fit::function<void(IoCommand*)> on_cancel) {
  for (TransactionData& txn_data : txns_) {
    if (!txn_data.active) {
      continue;
    }
    IoCommand* io_cmd = txn_data.io_cmd;
    ReleaseTransaction(txn_data);
    if (io_cmd != nullptr && on_cancel) {
      on_cancel(io_cmd);
    }
  }
}

void QueuePair::RingCompletionDb() {
  // Ring the doorbell.
  completion_doorbell_.set_value(static_cast<uint32_t>(completion_.NextIndex())).WriteTo(&mmio_);
}

zx::result<uint16_t> QueuePair::Submit(cpp20::span<uint8_t> submission_data,
                                       std::optional<zx::unowned_vmo> data_vmo, zx_off_t vmo_offset,
                                       size_t bytes, IoCommand* io_cmd) {
  if ((submission_.NextIndex() + 1) % submission_.entry_count() == sq_head_) {
    // No room. Try again later.
    return zx::error(ZX_ERR_SHOULD_WAIT);
  }
  if (submission_data.size() != sizeof(Submission)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // Allocate a free transaction slot decoupled from the submission queue index so a single
  // long-running command cannot block subsequent submissions when the submission ring wraps around.
  std::optional<size_t> index;
  for (size_t i = 0; i < txns_.size(); i++) {
    size_t candidate = (submission_.NextIndex() + i) % txns_.size();
    if (!txns_[candidate].active) {
      index = candidate;
      break;
    }
  }
  if (!index.has_value()) {
    return zx::error(ZX_ERR_SHOULD_WAIT);
  }
  TransactionData& txn_data = txns_[*index];
  txn_data.ClearExceptPrp();

  // We only peek here so that if the transaction setup fails somewhere we can easily roll-back
  auto submission = static_cast<Submission*>(submission_.Peek());
  // Copy provided data into place.
  memcpy(submission, submission_data.data(), submission_data.size());

  // We do not support metadata.
  submission->metadata_pointer = 0;
  submission->set_cid(static_cast<uint32_t>(*index)).set_fused(0).set_data_transfer_mode(0);

  if (data_vmo.has_value()) {
    // Checking this up front also means that the page count calculation below cannot overflow.
    if (bytes == 0 || bytes > kMaxTransferPages * kPageSize) {
      fdf::error("Invalid transfer size {} (must be non-zero and at most {} bytes).", bytes,
                 kMaxTransferPages * kPageSize);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    // Page offset of first page of transfer
    const size_t page_offset = vmo_offset & (~kPageMask);
    // Byte offset into first page of transfer
    const size_t byte_offset = vmo_offset & kPageMask;
    // The offset portion of a PRP entry must be dword aligned. See NVM Express Base Specification
    // 2.0 Section 4.1.1, "Physical Region Page Entry and List".
    if ((byte_offset & 0x3) != 0) {
      fdf::error("Data offset {} is not dword aligned.", vmo_offset);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    // Total pages mapped / touched. A transfer of the maximum size that does not start on a page
    // boundary touches kMaxPinnedPages pages.
    const size_t page_count = (byte_offset + bytes + kPageMask) >> kPageShift;
    ZX_DEBUG_ASSERT(page_count >= 1 && page_count <= kMaxPinnedPages);

    zx_paddr_t single_page;
    zx_paddr_t* page_list;
    if (io_cmd == nullptr) {
      if (page_count != 1) {
        fdf::error("Unexpected page count for admin command.");
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      page_list = &single_page;
    } else {
      if (!txn_data.prp_buffer) {
        fdf::error("No PRP buffer was preallocated for this IO transaction.");
        return zx::error(ZX_ERR_BAD_STATE);
      }
      page_list = static_cast<zx_paddr_t*>(txn_data.prp_buffer->virt());
    }

    // Read disk and write memory (PERM_WRITE), or read memory (PERM_READ) and write disk.
    const uint32_t options =
        submission->opcode() == IoCommandOpcode::kWrite ? ZX_BTI_PERM_READ : ZX_BTI_PERM_WRITE;
    // These get unpinned in CheckForNewCompletion().
    zx_status_t status = bti_->pin(options, *data_vmo.value(), page_offset,
                                   page_count << kPageShift, page_list, page_count, &txn_data.pmt);
    if (status != ZX_OK) {
      fdf::error("Failed to pin IO buffer: {}", zx_status_get_string(status));
      return zx::error(status);
    }

    fdf::trace("Submitting transaction #{} command {}: op={}, pages={}", *index,
               static_cast<const void*>(io_cmd), options == ZX_BTI_PERM_WRITE ? "RD" : "WR",
               page_count);

    submission->data_pointer[0] = page_list[0] | byte_offset;
    if (page_count == 2) {
      submission->data_pointer[1] = page_list[1];
    } else if (page_count > 2) {
      // See QueuePair::kMaxTransferPages.
      submission->data_pointer[1] = txn_data.prp_buffer->phys() + sizeof(zx_paddr_t*);
    }
  }

  // We used Peek() before, so advance the pointer, and mark the transaction as in-flight.
  submission_.Next();
  txn_data.io_cmd = io_cmd;
  txn_data.data_vmo = data_vmo;
  txn_data.active = true;
  ++in_flight_count_;

  // Ring the doorbell.
  submission_doorbell_.set_value(static_cast<uint32_t>(submission_.NextIndex())).WriteTo(&mmio_);
  return zx::ok(static_cast<uint16_t>(*index));
}

zx_status_t QueuePair::PreparePrpList(std::unique_ptr<dma_buffer::PagedBuffer>& buf,
                                      cpp20::span<const zx_paddr_t> pages) {
  const size_t addresses_per_page = zx_system_get_page_size() / sizeof(zx_paddr_t);
  size_t page_count = 0;
  // TODO(https://fxbug.dev/42053036): improve this in cases where we would allocate a page with
  // only one entry.
  page_count = pages.size() / (addresses_per_page - 1);
  page_count += 1;

  auto buffer_factory = dma_buffer::CreateBufferFactory();
  zx_status_t status = buffer_factory->CreatePaged(*bti_, page_count * zx_system_get_page_size(),
                                                   dma_buffer::CacheOptions::kDisabled, &buf);
  if (status != ZX_OK) {
    return status;
  }

  zx_paddr_t* addresses = static_cast<zx_paddr_t*>(buf->virt());
  size_t prp_index = 0;
  const zx_paddr_t* prp_list_pages = buf->phys();
  size_t prp_list_page_count = page_count;
  for (size_t i = 0; i < pages.size(); i++, prp_index++) {
    // If we're about to cross a page boundary, put the address of the next page here.
    if (prp_index % addresses_per_page == (addresses_per_page - 1)) {
      if (prp_list_page_count == 0) {
        fdf::error("Ran out of PRP pages?");
        return ZX_ERR_INTERNAL;
      }
      addresses[prp_index] = *prp_list_pages;
      prp_list_pages++;
      prp_list_page_count--;
      prp_index++;
    }

    addresses[prp_index] = pages[i];
  }

  return ZX_OK;
}

}  // namespace nvme
