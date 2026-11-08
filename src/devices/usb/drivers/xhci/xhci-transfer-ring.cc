// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/xhci-transfer-ring.h"

#include "src/devices/usb/drivers/xhci/usb-xhci.h"

namespace usb_xhci {

TRB* TransferRing::AdvancePointerHelper(TRB* trb) {
  TRB* ptr = trb ? trb : trbs_;
  if ((reinterpret_cast<size_t>(ptr) / zx_system_get_page_size()) !=
      (reinterpret_cast<size_t>(ptr + 1) / zx_system_get_page_size())) {
    CommitLocked();
    auto buffer =
        virt_to_buffer_.find((reinterpret_cast<size_t>(ptr) / zx_system_get_page_size()) + 1);
    if (buffer == virt_to_buffer_.end()) {
      return nullptr;
    }
    ptr = static_cast<TRB*>(buffer->second->virt());
  } else {
    ptr++;
  }

  // In some cases empty segments (segments containing only a link TRB)
  // may be generated. When advancing to the next segment, this needs to
  // account for empty segments.
  // See the test "CanHandleConsecutiveLinks" in xhci-transfer-ring-test.cc for an
  // example that triggers this scenario.
  Control control = Control::FromTRB(ptr);
  while (control.Type() == Control::Link) {
    zx_paddr_t addr = ptr->ptr;
    if (!trb) {
      control.set_Cycle(pcs_).ToTrb(ptr);
      // Read link pointer
      if (control.EntTC()) {
        pcs_ = !pcs_;
      }
      if (!hci_->HasCoherentState()) {
        InvalidatePageCache(ptr, ZX_CACHE_FLUSH_DATA);
      }
    }
    ptr = PhysToVirtLocked(addr);
    if (!ptr) {
      return nullptr;
    }
    control = Control::FromTRB(ptr);
  }

  return ptr;
}

zx_status_t TransferRing::AllocInternal(Control control) {
  // Reserve two additional TRBs for expanding the ring
  if (!AvailableSlots(2)) {
    // Attempt to grow the ring
    dma_buffer::ContiguousBuffer* new_trb;
    zx_status_t status = AllocBuffer(&new_trb);
    if (status != ZX_OK) {
      return status;
    }
    auto link_state = SaveStateLocked();
    TRB* link_trb = trbs_;
    control.set_Type(Control::Nop).ToTrb(trbs_);
    // Advance to the next spare TRB (this will go AFTER the link TRB)
    // Update size of ring
    // NOTE: This might bring us to a link TRB.
    // We shouldn't overwrite an existing link TRB.
    // It should be swapped with the new buffer instead
    capacity_ += (*new_trb).size() / sizeof(TRB);
    trbs_++;
    TRB* spare_trb = trbs_;
    Control spare_control = Control::FromTRB(spare_trb);
    TRB* new_link_trb =
        static_cast<TRB*>((*new_trb).virt()) + ((*new_trb).size() / sizeof(TRB)) - 1;
    if (spare_control.Type() == Control::Link) {
      // Special case for link TRBs. Simply swap links and update.
      new_link_trb->ptr = spare_trb->ptr;
      if (spare_control.EntTC()) {
        // Special case -- append new segment to last TRB (requires PCS toggle)
        pcs_ = !pcs_;
      }
      Control::Get()
          .FromValue(0)
          .set_Type(Control::Link)
          .set_EntTC(spare_control.EntTC())
          .set_Cycle(!pcs_)
          .ToTrb(new_link_trb);
      if (!hci_->HasCoherentState()) {
        InvalidatePageCache(new_link_trb, ZX_CACHE_FLUSH_DATA);
      }
      hw_mb();
      spare_trb->ptr = (*new_trb).phys();
      spare_control.set_EntTC(0).ToTrb(spare_trb);
      if (!hci_->HasCoherentState()) {
        InvalidatePageCache(spare_trb, ZX_CACHE_FLUSH_DATA);
      }
      RestoreLocked(link_state);
      return ZX_OK;
    }
    // Update pointer from new buffer to point to the spare TRB
    ZX_ASSERT((*new_trb).size() == zx_system_get_page_size());
    new_link_trb->ptr = VirtToPhysLocked(spare_trb);
    Control::Get()
        .FromValue(0)
        .set_Type(Control::Link)
        .set_EntTC(0)
        .set_Cycle(!pcs_)
        .ToTrb(new_link_trb);
    if (!hci_->HasCoherentState()) {
      InvalidatePageCache(new_link_trb, ZX_CACHE_FLUSH_DATA);
    }
    // Update pointer to new segment (adding the new segment to the ring)
    link_trb->ptr = (*new_trb).phys();
    link_trb->status = 0;
    // Restore the previous state
    RestoreLocked(link_state);
    hw_mb();
    Control::Get()
        .FromValue(0)
        .set_Type(Control::Link)
        .set_Cycle(pcs_)
        .set_EntTC(0)
        .ToTrb(link_trb);
    // Advance into the new segment
    // Advance two spots, one for the link TRB, and one for the new TRB
    // PCS stays the same across this transition.
    CommitLocked();
    trbs_ = static_cast<TRB*>((*new_trb).virt());
    ZX_ASSERT(Control::FromTRB(trbs_).Type() != Control::Link);
    ZX_ASSERT(trbs_ == (*new_trb).virt());
    return ZX_OK;
  }
  if (!AvailableSlots(1)) {
    return ZX_ERR_BAD_STATE;
  }
  return ZX_OK;
}

void TransferRing::Commit() {
  if (!hci_->HasCoherentState()) {
    fbl::AutoLock l(&mutex_);
    CommitLocked();
  }
}

void TransferRing::CommitLocked() {
  if (!hci_->HasCoherentState()) {
    InvalidatePageCache(trbs_, ZX_CACHE_FLUSH_DATA);
  }
}

void TransferRing::CommitTransaction(const State& start) {
  if (!hci_->HasCoherentState()) {
    fbl::AutoLock l(&mutex_);
    if (!start.trbs) {
      return;
    }
    const size_t page_size = zx_system_get_page_size();
    const uintptr_t first_page = reinterpret_cast<uintptr_t>(start.trbs) / page_size;
    uintptr_t current_page = first_page;
    bool ccs = start.pcs;
    TRB* current = start.trbs;
    while (current && Control::FromTRB(current).Cycle() == ccs) {
      auto control = Control::FromTRB(current);
      if (control.Type() == Control::Link) {
        if (control.EntTC()) {
          ccs = !ccs;
        }
        if (current_page != first_page) {
          InvalidatePageCache(current, ZX_CACHE_FLUSH_DATA);
        }
        current = PhysToVirtLocked(current->ptr);
        if (!current) {
          InvalidatePageCache(start.trbs, ZX_CACHE_FLUSH_DATA);
          return;
        }
        current_page = reinterpret_cast<uintptr_t>(current) / page_size;
      } else {
        current++;
      }
    }
    if (current && current_page != first_page) {
      InvalidatePageCache(current, ZX_CACHE_FLUSH_DATA);
    }
    InvalidatePageCache(start.trbs, ZX_CACHE_FLUSH_DATA);
  }
}

zx_status_t TransferRing::AddTRB(const TRB& trb, std::unique_ptr<TRBContext> context) {
  fbl::AutoLock l(&mutex_);
  if (context->token != token_) {
    return ZX_ERR_INVALID_ARGS;
  }
  Control control = Control::FromTRB(trbs_);
  zx_status_t status = AllocInternal(control);
  if (status != ZX_OK) {
    return status;
  }
  if (Control::FromTRB(trbs_).Type() == Control::Link) {
    return ZX_ERR_BAD_STATE;
  }
  context->trb = trbs_;
  control = Control::Get().FromValue(trb.control);
  control.set_Cycle(pcs_);
  trbs_->ptr = trb.ptr;
  trbs_->status = 0;
  // Control must be the last thing to be written -- to ensure that ptr points to a valid location
  // in memory
  hw_mb();
  control.ToTrb(trbs_);
  hw_mb();
  AdvancePointer();
  pending_trbs_.push_back(std::move(context));
  if (!hci_->HasCoherentState()) {
    InvalidatePageCache(trbs_, ZX_CACHE_FLUSH_DATA);
  }
  return ZX_OK;
}

zx_status_t TransferRing::HandleShortPacket(TRB* short_trb, size_t short_length, TRB** last_trb) {
  fbl::AutoLock l(&mutex_);
  if (pending_trbs_.is_empty() || !short_trb) {
    return ZX_ERR_IO;
  }
  auto target_ctx = pending_trbs_.begin();
  if (target_ctx->short_transfer_len.has_value()) {
    // Controller gave us a duplicate event. Discard it but report an error.
    // This is non-fatal and may happen frequently on certain controllers.
    return ZX_OK;
  }
  auto end_ctx = target_ctx;
  end_ctx++;
  const TRB* end = nullptr;
  if (end_ctx != pending_trbs_.end()) {
    end = end_ctx->first_trb;
  }
  TRB* current = target_ctx->first_trb;
  size_t transferred = 0;
  while (current != end) {
    transferred += static_cast<Normal*>(current)->LENGTH();
    if (current == short_trb) {
      *last_trb = target_ctx->trb;
      target_ctx->short_transfer_len = transferred - short_length;
      return ZX_OK;
    }
    size_t page = reinterpret_cast<size_t>(current) / zx_system_get_page_size();
    current++;
    if ((reinterpret_cast<size_t>(current) / zx_system_get_page_size()) != page) {
      return ZX_ERR_IO;
    }
    while (Control::FromTRB(current).Type() == Control::Link) {
      current = PhysToVirtLocked(current->ptr);
      if (!current) {
        return ZX_ERR_IO;
      }
    }
  }
  return ZX_ERR_IO;
}

zx_status_t TransferRing::AssignContext(TRB* trb, std::unique_ptr<TRBContext> context,
                                        TRB* first_trb, TRB* setup_trb) {
  fbl::AutoLock l(&mutex_);
  if (context->token != token_) {
    return ZX_ERR_INVALID_ARGS;
  }
  zx_status_t status = AllocInternal(Control::FromTRB(trbs_));
  if (status != ZX_OK) {
    return status;
  }
  context->first_trb = first_trb;
  context->trb = trb;
  context->setup_trb = setup_trb;
  pending_trbs_.push_back(std::move(context));
  return ZX_OK;
}

TransferRing::State TransferRing::SaveState() {
  fbl::AutoLock l(&mutex_);
  return SaveStateLocked();
}
TransferRing::State TransferRing::SaveStateLocked() {
  State state;
  state.pcs = pcs_;
  state.trbs = trbs_;
  return state;
}
void TransferRing::Restore(const State& state) {
  fbl::AutoLock l(&mutex_);
  RestoreLocked(state);
}
void TransferRing::RestoreLocked(const State& state) {
  trbs_ = state.trbs;
  pcs_ = state.pcs;
}

zx_status_t TransferRing::Init(size_t page_size, const zx::bti& bti, EventRing* ring, bool is_32bit,
                               fdf::MmioBuffer* mmio, UsbXhci* hci) {
  fbl::AutoLock l(&mutex_);
  if (trbs_ != nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  page_size_ = page_size;
  bti_ = &bti;
  ring_ = ring;
  is_32_bit_ = is_32bit;
  mmio_ = mmio;
  dma_buffer::ContiguousBuffer* container;
  isochronous_ = false;
  token_++;
  hci_ = hci;
  stalled_ = false;
  return AllocBuffer(&container);
}

zx_status_t TransferRing::DeinitIfActive() {
  fbl::AutoLock l(&mutex_);
  if (!trbs_) {
    return ZX_OK;
  }
  return DeinitLocked();
}
zx_status_t TransferRing::Deinit() {
  fbl::AutoLock l(&mutex_);
  if (!trbs_) {
    return ZX_ERR_BAD_STATE;
  }
  return DeinitLocked();
}
zx_status_t TransferRing::DeinitLocked() {
  trbs_ = nullptr;
  dequeue_trb_ = nullptr;
  pcs_ = true;
  buffers_.clear();
  virt_to_buffer_.clear();
  isochronous_ = false;
  phys_to_buffer_.clear();
  if (ring_) {
    ring_->RemovePressure();
  }
  return ZX_OK;
}

CRCR TransferRing::TransferRing::phys(uint8_t cap_length) {
  fbl::AutoLock l(&mutex_);
  CRCR cr = CRCR::Get(cap_length).FromValue(trb_start_phys_);
  ZX_ASSERT(trb_start_phys_);
  cr.set_RCS(pcs_);
  return cr;
}

zx::result<CRCR> TransferRing::TransferRing::PeekCommandRingControlRegister(uint8_t cap_length) {
  fbl::AutoLock l(&mutex_);
  Control control = Control::FromTRB(trbs_);
  zx_status_t status = AllocInternal(control);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  CRCR cr = CRCR::Get(cap_length).FromValue(VirtToPhysLocked(trbs_));
  ZX_ASSERT(trb_start_phys_);
  cr.set_RCS(pcs_);
  return zx::ok(cr);
}

zx_paddr_t TransferRing::VirtToPhys(TRB* trb) {
  fbl::AutoLock l(&mutex_);
  return VirtToPhysLocked(trb);
}

zx_paddr_t TransferRing::VirtToPhysLocked(TRB* trb) {
  const auto& buffer =
      virt_to_buffer_.find(reinterpret_cast<zx_vaddr_t>(trb) / zx_system_get_page_size());
  if (buffer == virt_to_buffer_.end()) {
    return 0;
  }
  auto offset = reinterpret_cast<zx_vaddr_t>(trb) % zx_system_get_page_size();
  return buffer->second->phys() + offset;
}

TRB* TransferRing::PhysToVirt(zx_paddr_t paddr) {
  fbl::AutoLock l(&mutex_);
  return PhysToVirtLocked(paddr);
}

TRB* TransferRing::PhysToVirtLocked(zx_paddr_t paddr) {
  const auto& buffer = phys_to_buffer_.find(paddr / zx_system_get_page_size());
  if (buffer == phys_to_buffer_.end()) {
    return nullptr;
  }
  auto offset = paddr % zx_system_get_page_size();
  auto vaddr = reinterpret_cast<zx_vaddr_t>(buffer->second->virt()) + offset;
  return reinterpret_cast<TRB*>(vaddr);
}

bool TransferRing::IsTRBInRange(TRB* trb, TRB* start, TRB* end) {
  if (!trb) {
    return false;
  }

  if (trb == start || trb == end) {
    return true;
  }

  if (!start || !end) {
    return false;
  }

  for (auto* cur = start; cur != end && cur != nullptr; AdvancePointer(cur)) {
    if (cur == trb) {
      return true;
    }
  }

  return false;
}

zx_status_t TransferRing::CompleteTRB(TRB* trb, std::unique_ptr<TRBContext>* context) {
  fbl::AutoLock l(&mutex_);
  if (pending_trbs_.is_empty()) {
    fdf::error("Pending TRB list is empty but we received a completion event");
    return ZX_ERR_CANCELED;
  }

  *context = pending_trbs_.pop_front();
  if (trb == nullptr || (!IsTRBInRange(trb, (*context)->first_trb, (*context)->trb) &&
                         (trb != (*context)->setup_trb))) {
    fdf::error("Lost a TRB! Expected {} but we received an event for {}",
               static_cast<const void*>((*context)->trb), static_cast<const void*>(trb));
    return ZX_ERR_IO;
  }
  dequeue_trb_ = trb;

  if (trb != (*context)->trb) {
    fdf::warn(
        "Received a completion event in response to not the last TRB."
        " This transaction should have failed! Please double check.");
  }

  return ZX_OK;
}

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBs() {
  fbl::AutoLock l(&mutex_);
  return std::move(pending_trbs_);
}

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBsUntil(TRB* end) {
  fbl::AutoLock l(&mutex_);
  dequeue_trb_ = end;
  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> retval;
  while (!pending_trbs_.is_empty()) {
    if (pending_trbs_.front().trb == end) {
      break;
    }
    auto trb = pending_trbs_.pop_front();
    retval.push_back(std::move(trb));
  }
  return retval;
}

zx_status_t TransferRing::AllocateTRB(TRB** trb, State* state) {
  fbl::AutoLock l(&mutex_);
  if (state) {
    state->pcs = pcs_;
    state->trbs = trbs_;
  }
  Control control = Control::FromTRB(trbs_);
  zx_status_t status = AllocInternal(control);
  if (status != ZX_OK) {
    return status;
  }
  if (Control::FromTRB(trbs_).Type() == Control::Link) {
    return ZX_ERR_BAD_STATE;
  }
  trbs_->ptr = 0;
  trbs_->status = pcs_;
  *trb = trbs_;
  {
    TRB empty;
    empty.control = 0;
    bool pcs = Control::FromTRB(*trb).Cycle();
    Control::FromTRB(&empty).set_Cycle(pcs).ToTrb(*trb);
  }
  AdvancePointer();
  return ZX_OK;
}

zx::result<ContiguousTRBInfo> TransferRing::AllocateContiguous(size_t count) {
  if (count == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  size_t max_contiguous = 0;
  {
    fbl::AutoLock l(&mutex_);
    if (page_size_ < sizeof(TRB) * 3) {
      return zx::error(ZX_ERR_BAD_STATE);
    }
    max_contiguous = (page_size_ / sizeof(TRB)) - 2;
  }
  if (count > max_contiguous) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  size_t orig_count = count;
  TRB* contig = nullptr;
  TRB* nop;
  size_t nop_count = 1;
  zx_status_t status = AllocateTRB(&nop, nullptr);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  Control::FromTRB(nop).set_Type(Control::Nop).ToTrb(nop);
  TRB* prev = nop;
  count--;
  bool discontiguous = false;
  while (count) {
    TRB* current;
    status = AllocateTRB(&current, nullptr);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    if (current != prev + 1) {
      discontiguous = true;
      contig = current;
      prev = nullptr;
      break;
    }
    nop_count++;
    Control::FromTRB(current).set_Type(Control::Nop).ToTrb(current);
    prev = current;
    count--;
  }
  if (discontiguous) {
    // Discontiguous -- start another run
    count = orig_count - 1;
    prev = contig;
    while (count) {
      TRB* current;
      status = AllocateTRB(&current, nullptr);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      // Still discontiguous (couldn't get enough contiguous physical memory)
      if (current != prev + 1) {
        // NOTE: Today we can't guarantee the availability of contiguous physical memory,
        // so we'll bail out if we can't satisfy the request.
        fdf::error(
            "No physically contiguous memory available to satisfy TRB allocation request.\n");
        return zx::error(ZX_ERR_NO_MEMORY);
      }
      prev = current;
      count--;
    }
    ContiguousTRBInfo info;
    info.nop = cpp20::span<TRB>(nop, nop_count);
    info.trbs = cpp20::span<TRB>(contig, orig_count);
    return zx::ok(info);
  }
  // Already contiguous
  ContiguousTRBInfo info;
  info.trbs = cpp20::span<TRB>(nop, orig_count);
  return zx::ok(info);
}

zx_status_t TransferRing::AllocBuffer(dma_buffer::ContiguousBuffer** out) {
  std::unique_ptr<dma_buffer::ContiguousBuffer> buffer;
  {
    std::unique_ptr<dma_buffer::ContiguousBuffer> buffer_tmp;
    zx_status_t status = hci_->buffer_factory().CreateContiguous(
        *bti_, page_size_,
        static_cast<uint32_t>(page_size_ == zx_system_get_page_size() ? 0 : page_size_ >> 12),
        dma_buffer::CacheOptions::kEnabled, &buffer_tmp);
    if (status != ZX_OK) {
      return status;
    }
    buffer = std::move(buffer_tmp);
  }
  if (is_32_bit_ && (buffer->phys() >= UINT32_MAX)) {
    return ZX_ERR_NO_MEMORY;
  }
  zx_vaddr_t virt = reinterpret_cast<zx_vaddr_t>(buffer->virt());
  zx_paddr_t phys = buffer->phys();
  const size_t count = page_size_ / sizeof(TRB);
  TRB* trbs = reinterpret_cast<TRB*>(virt);
  Control control = Control::Get().FromValue(0);
  control.set_Type(Control::Link);
  if (!trbs_) {
    trbs_ = trbs;
    capacity_ = count;
    trb_start_phys_ = phys;
    dequeue_trb_ = trbs_;
  }
  control.set_EntTC(1);
  trbs[count - 1].ptr = trb_start_phys_;
  hw_mb();
  control.ToTrb(trbs + (count - 1));
  zx_status_t status = ring_->AddSegmentIfNoneLock();
  if (status != ZX_OK) {
    return status;
  }
  virt_to_buffer_[virt / zx_system_get_page_size()] = buffer.get();
  phys_to_buffer_[phys / zx_system_get_page_size()] = buffer.get();
  *out = buffer.get();
  buffers_.push_back(std::move(buffer));
  if (!pcs_) {
    for (size_t i = 0; i < count; i++) {
      Control::FromTRB(trbs + i).set_Cycle(1).ToTrb(trbs + i);
    }
  }
  zx_cache_flush(reinterpret_cast<void*>(virt), page_size_, ZX_CACHE_FLUSH_DATA);
  return ZX_OK;
}

bool TransferRing::AvailableSlots(size_t count) {
  TRB* current = trbs_ + 1;
  while (count) {
    if (current == dequeue_trb_) {
      return false;
    }
    Control control = Control::FromTRB(current);
    if (control.Type() == Control::Link) {
      current = PhysToVirtLocked(current->ptr);
      if (!current) {
        return false;
      }
      // Don't count link TRBs as available slots. We can't actually
      // put data in them.
      continue;
    }
    current++;
    count--;
  }
  return true;
}

}  // namespace usb_xhci
