// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/media/codec_impl/codec_buffer.h>
#include <lib/media/codec_impl/codec_impl.h>
#include <lib/media/codec_impl/codec_port.h>
#include <lib/media/codec_impl/log.h>
#include <lib/memory_barriers/memory_barriers.h>
#include <zircon/assert.h>
#include <zircon/types.h>

#include <fbl/algorithm.h>

CodecBuffer::CodecBuffer(CodecImpl* parent, Info buffer_info, CodecVmoRange vmo_range)
    : parent_(parent), buffer_info_(std::move(buffer_info)), vmo_range_(std::move(vmo_range)) {
  zx_info_vmo vmo_info{};
  zx_status_t get_info_status =
      vmo_range_.vmo().get_info(ZX_INFO_VMO, &vmo_info, sizeof(vmo_info), nullptr, nullptr);
  ZX_ASSERT(get_info_status == ZX_OK);
  raw_vmo_size_ = vmo_info.size_bytes;
  ZX_ASSERT(vmo_offset() <= raw_vmo_size_ && size() <= raw_vmo_size_ - vmo_offset());
  zx_status_t vmo_status =
      vmo_range_.vmo().create_child(ZX_VMO_CHILD_SLICE, 0, raw_vmo_size_, &vmo_);
  ZX_ASSERT(vmo_status == ZX_OK);
  zx_status_t parent_status =
      vmo_range_.vmo().create_child(ZX_VMO_CHILD_SLICE, 0, raw_vmo_size_, &parent_vmo_);
  ZX_ASSERT(parent_status == ZX_OK);
  zx::vmo until_remove_started_child_vmo;
  zx_status_t until_remove_started_status = parent_vmo_.create_child(
      ZX_VMO_CHILD_SLICE, 0, raw_vmo_size_, &until_remove_started_child_vmo);
  ZX_ASSERT(until_remove_started_status == ZX_OK);
  {
    std::scoped_lock lock(until_remove_started_child_vmo_lock_);
    until_remove_started_child_vmo_ =
        std::make_shared<zx::vmo>(std::move(until_remove_started_child_vmo));
  }
  zero_children_wait_.emplace(this, parent_vmo_.get(), ZX_VMO_ZERO_CHILDREN);
}

CodecBuffer::~CodecBuffer() {
  VLOGF("codec_buffer: %p", this);
  zx_status_t status;
  if (is_mapped_) {
    ZX_DEBUG_ASSERT(buffer_base_);
    uint64_t adjusted_vmo_offset = fbl::round_down(vmo_offset(), zx_system_get_page_size());
    uintptr_t unmap_address =
        fbl::round_down(reinterpret_cast<uintptr_t>(base()), zx_system_get_page_size());
    size_t unmap_len = raw_vmo_size_ - adjusted_vmo_offset;
    status = zx::vmar::root_self()->unmap(unmap_address, unmap_len);
    if (status != ZX_OK) {
      parent_->FailFatal("CodecBuffer::~CodecBuffer() failed to unmap() Buffer - status: %d",
                         status);
    }
    buffer_base_ = nullptr;
    is_mapped_ = false;
  }
  if (pinned_) {
    status = pinned_.unpin();
    ZX_ASSERT(status == ZX_OK);
    ZX_ASSERT(!pinned_);
    if (status != ZX_OK) {
      parent_->FailFatal("CodecBuffer::~CodecBuffer() failed unpin() - status: %d", status);
    }
  }
  magic_ = 0xfefefefe;
}

void CodecBuffer::SetDoDelete(DoDelete do_delete) { do_delete_ = std::move(do_delete); }

void CodecBuffer::BeginWaitForZeroChildren(async_dispatcher_t* dispatcher) {
  zero_children_wait_->Begin(dispatcher);
}

bool CodecBuffer::Map() {
  ZX_DEBUG_ASSERT(!buffer_info_.is_secure);
  // Map the VMO in the local address space.
  uintptr_t tmp;
  zx_vm_option_t flags = ZX_VM_PERM_READ;
  if (buffer_info_.port == kOutputPort) {
    flags |= ZX_VM_PERM_WRITE;
  }

  // We must page-align the mapping (since HW can only map at page granularity).  This means the
  // mapping may include up to PAGE_SIZE - 1 bytes before vmo_usable_start, and up to
  // raw_vmo_size_.  The meaningful content usage of the mapping is expected to stay within
  // CodecBuffer::base() to CodecBuffer::base() + size(), while padding-only accesses may extend up
  // to raw_vmo_size_.
  uint64_t adjusted_vmo_offset = fbl::round_down(vmo_offset(), zx_system_get_page_size());
  size_t len = raw_vmo_size_ - adjusted_vmo_offset;
  zx_status_t res = zx::vmar::root_self()->map(flags, 0, vmo(), adjusted_vmo_offset, len, &tmp);
  if (res != ZX_OK) {
    LOG(ERROR, "Failed to map %zu byte buffer vmo (res %d)", size(), res);
    return false;
  }
  buffer_base_ = reinterpret_cast<uint8_t*>(tmp + (vmo_offset() % zx_system_get_page_size()));
  is_mapped_ = true;
  return true;
}

void CodecBuffer::FakeMap(uint8_t* fake_map_addr) {
  ZX_DEBUG_ASSERT(reinterpret_cast<uintptr_t>(fake_map_addr) % zx_system_get_page_size() == 0);
  buffer_base_ = fake_map_addr + (vmo_offset() % zx_system_get_page_size());
  ZX_DEBUG_ASSERT(!is_mapped_);
}

uint8_t* CodecBuffer::base() const {
  ZX_DEBUG_ASSERT(buffer_base_ && "Shouldn't be using if buffer was not mapped.");
  return buffer_base_;
}

bool CodecBuffer::is_known_contiguous() const { return is_known_contiguous_; }

zx_paddr_t CodecBuffer::physical_base() const {
  // Must call Pin() first.
  ZX_DEBUG_ASSERT(pinned_);
  // Else we'll need a different method that can deal with scattered pages.  For now we don't need
  // that.
  ZX_DEBUG_ASSERT(is_known_contiguous_);
  return contiguous_paddr_base_;
}

size_t CodecBuffer::size() const { return vmo_range_.size(); }

size_t CodecBuffer::raw_vmo_size() const { return raw_vmo_size_; }

const zx::vmo& CodecBuffer::vmo() const { return vmo_; }

zx::vmo CodecBuffer::GetChildVmo() const {
  std::shared_ptr<zx::vmo> local_vmo;
  {
    std::scoped_lock lock(until_remove_started_child_vmo_lock_);
    local_vmo = until_remove_started_child_vmo_;
  }
  if (local_vmo) {
    // duplicate() is lower overhead than create_child(), which at this point is the only
    // remaining reason this path exists.
    zx::vmo dup;
    zx_status_t dup_status = local_vmo->duplicate(ZX_RIGHT_SAME_RIGHTS, &dup);
    ZX_ASSERT_MSG(dup_status == ZX_OK, "dup_status: %s", zx_status_get_string(dup_status));
    return dup;
  }
  return CreateChildVmoFromParent();
}

zx::vmo CodecBuffer::CreateChildVmoFromParent() const {
  // When a CodecAdapter that supports dynamic buffers retains a buffer beyond
  // CoreCodecRemoveBuffer / EnsureBuffersNotConfigured (by holding a handle obtained earlier from
  // GetChildVmo()) and subsequently emits an output packet referencing this buffer via
  // CodecPacket::SetBuffer(), GetKeepAlive() / GetChildVmo() can be called after
  // until_remove_started_child_vmo_ has already been reset. Because the CodecAdapter still holds
  // at least one child VMO of parent_vmo_, parent_vmo_ has not yet seen ZX_VMO_ZERO_CHILDREN, and
  // we can create a new child slice of parent_vmo_ to keep parent_vmo_ alive until the packet is
  // recycled.
  // Intentionally a release ZX_ASSERT (not ZX_DEBUG_ASSERT) to guard against buggy CodecAdapter(s)
  // that call GetChildVmo() / SetBuffer() after dropping all previously-obtained child handles.
  zx_info_vmo vmo_info{};
  zx_status_t get_info_status =
      parent_vmo_.get_info(ZX_INFO_VMO, &vmo_info, sizeof(vmo_info), nullptr, nullptr);
  ZX_ASSERT(get_info_status == ZX_OK);
  ZX_ASSERT(vmo_info.num_children > 0);
  zx::vmo child_vmo;
  zx_status_t child_status =
      parent_vmo_.create_child(ZX_VMO_CHILD_SLICE, 0, raw_vmo_size_, &child_vmo);
  ZX_ASSERT(child_status == ZX_OK);
  return child_vmo;
}

uint64_t CodecBuffer::vmo_offset() const { return vmo_range_.offset(); }

void CodecBuffer::SetVideoFrame(std::weak_ptr<VideoFrame> video_frame) const {
  deprecated_video_frame_ = video_frame;
}

std::weak_ptr<VideoFrame> CodecBuffer::video_frame() const { return deprecated_video_frame_; }

zx_status_t CodecBuffer::Pin() {
  if (is_pinned()) {
    return ZX_OK;
  }

  zx_info_vmo_t info;
  zx_status_t status = vmo().get_info(ZX_INFO_VMO, &info, sizeof(info), nullptr, nullptr);
  if (status != ZX_OK) {
    return status;
  }
  if (!(info.flags & ZX_INFO_VMO_CONTIGUOUS)) {
    // Not supported yet.
    return ZX_ERR_NOT_SUPPORTED;
  }
  // We could potentially know this via the BufferCollectionInfo_2, but checking the VMO directly
  // also works fine.
  is_known_contiguous_ = true;

  // We must page-align the pin (since pinning is page granularity).  This means the pin may include
  // up to PAGE_SIZE - 1 bytes before vmo_usable_start, and up to raw_vmo_size_.  The meaningful
  // content usage of the pin is expected to stay within CodecBuffer::physical_base() to
  // CodecBuffer::physical_base() + size(), while padding-only accesses may extend up to VMO offset
  // raw_vmo_size_.
  uint64_t pin_offset = fbl::round_down(vmo_offset(), zx_system_get_page_size());
  uint64_t pin_size = raw_vmo_size_ - pin_offset;

  uint32_t options = ZX_BTI_CONTIGUOUS | ZX_BTI_PERM_READ;
  if (port() == kOutputPort) {
    options |= ZX_BTI_PERM_WRITE;
  }

  zx_paddr_t paddr;
  status = parent_->Pin(options, vmo(), pin_offset, pin_size, &paddr, 1, &pinned_);
  if (status != ZX_OK) {
    return status;
  }
  // Include the low-order bits of vmo_usable_start() in contiguous_paddr_base_ so that the paddr
  // at contiguous_paddr_base_ points (physical) at the byte at offset vmo_usable_start() within
  // *vmo.
  contiguous_paddr_base_ = paddr + (vmo_offset() % zx_system_get_page_size());
  return ZX_OK;
}

bool CodecBuffer::is_pinned() const { return !!pinned_; }

void CodecBuffer::CacheFlush(uint32_t flush_offset, uint32_t length) const {
  CacheFlushInternal(flush_offset, length, /*also_invalidate*/ false);
}

void CodecBuffer::CacheFlushAndInvalidate(uint32_t flush_offset, uint32_t length) const {
  CacheFlushInternal(flush_offset, length, /*also_invalidate=*/true);
}

void CodecBuffer::CacheFlushInternal(uint32_t flush_offset, uint32_t length,
                                     bool also_invalidate) const {
  ZX_DEBUG_ASSERT(!is_secure());
  if (also_invalidate) {
    BarrierBeforeInvalidate();
  }
  zx_status_t status;
  if (is_mapped_) {
    uint32_t flush_type =
        also_invalidate ? ZX_CACHE_FLUSH_INVALIDATE | ZX_CACHE_FLUSH_DATA : ZX_CACHE_FLUSH_DATA;
    status = zx_cache_flush(base() + flush_offset, length, flush_type);
    if (status != ZX_OK) {
      ZX_PANIC("zx_cache_flush() failed - status: %d", status);
    }
  } else {
    uint32_t flush_type =
        also_invalidate ? ZX_VMO_OP_CACHE_CLEAN_INVALIDATE : ZX_VMO_OP_CACHE_CLEAN;
    status = vmo().op_range(flush_type, vmo_offset() + flush_offset, length, nullptr, 0);
    if (status != ZX_OK) {
      ZX_ASSERT_MSG(status == ZX_OK, "vmo().op_range() failed - status: %d", status);
    }
  }
  BarrierAfterFlush();
}

void CodecBuffer::OnZeroChildren(async_dispatcher_t* dispatcher, async::WaitBase* wait,
                                 zx_status_t status, const zx_packet_signal_t* signal) {
  if (status == ZX_ERR_CANCELED) {
    VLOGF("status == ZX_ERR_CANCELED - codec_buffer: %p", this);
    // forced destruction; do nothing
    return;
  }
  VLOGF("normal (not cancelled) - codec_buffer: %p", this);
  ZX_DEBUG_ASSERT(status == ZX_OK);
  ZX_DEBUG_ASSERT(signal->trigger & ZX_VMO_ZERO_CHILDREN);
  // some tests don't use SetDoDelete
  auto local_do_delete = std::move(do_delete_);
  ZX_DEBUG_ASSERT(!do_delete_);
  if (local_do_delete) {
    // will delete "this"
    std::move(local_do_delete)(this);
    ZX_DEBUG_ASSERT(!local_do_delete);
  }
}

CodecBuffer::KeepAlive CodecBuffer::GetKeepAlive() const {
  {
    std::scoped_lock lock(until_remove_started_child_vmo_lock_);
    if (until_remove_started_child_vmo_) {
      return KeepAlive(until_remove_started_child_vmo_);
    }
  }
  return KeepAlive(std::make_shared<zx::vmo>(CreateChildVmoFromParent()));
}

void CodecBuffer::ResetUntilRemoveStartedChildVmo() {
  std::shared_ptr<zx::vmo> to_drop;
  {
    std::scoped_lock lock(until_remove_started_child_vmo_lock_);
    if (until_remove_started_child_vmo_) {
      VLOGF("until_remove_started_child_vmo_.reset() - port: %u buffer: %p", port(), this);
      to_drop = std::move(until_remove_started_child_vmo_);
    }
  }
}

bool CodecBuffer::HasUntilRemoveStartedChildVmoForDebug() const {
  std::scoped_lock lock(until_remove_started_child_vmo_lock_);
  return static_cast<bool>(until_remove_started_child_vmo_);
}

fit::function<void(ScopedLock&)> CodecBuffer::TakePendingRemoveCompletion() {
  return std::move(pending_remove_completion_);
}
