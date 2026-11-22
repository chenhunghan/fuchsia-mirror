// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/renderer/batch_gpu_uploader.h"

#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>

#include <limits>
#include <optional>
#include <variant>

#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/util/align.h"
#include "src/ui/lib/escher/util/image_utils.h"
#include "src/ui/lib/escher/util/trace_macros.h"

namespace escher {

namespace {

// Vulkan specs requires that bufferOffset in VkBufferImageCopy be multiple of
// 4, so we enforce that all buffer offsets (including buffers and images) align
// to multiples of 4 by aligning up the addresses.
constexpr vk::DeviceSize kByteAlignment = 4;

// Returns true for the formats `ScheduleWriteImage()` accepts: single-plane color formats, whose
// copy is one color-aspect region of `width * height * bytes-per-pixel` bytes, and whose texel
// size divides `kByteAlignment`, so that the 4-byte-aligned `bufferOffset` is valid. Multi-planar
// YUV formats are excluded because they need one copy region per plane. `eG8B8G8R8422Unorm` meets
// the definition but is left out because nothing uploads it and no test covers it. The list must
// stay a subset of the formats `image_utils::GetImageByteSize()` handles; a format missing there
// crashes in that function's `FX_CHECK` on the first upload. The format list in the
// `ScheduleWriteImage()` comment in `batch_gpu_uploader.h` and the `kFormatCases` table in
// `batch_gpu_uploader_unittest.cc` must change with this `switch`.
bool IsUploadFormatSupported(vk::Format format) {
  switch (format) {
    case vk::Format::eR8G8B8A8Unorm:
    case vk::Format::eR8G8B8A8Srgb:
    case vk::Format::eB8G8R8A8Unorm:
    case vk::Format::eB8G8R8A8Srgb:
    case vk::Format::eR8Unorm:
      return true;
    default:
      return false;
  }
}

}  // namespace

std::unique_ptr<BatchGpuUploader> BatchGpuUploader::New(EscherWeakPtr weak_escher,
                                                        uint64_t frame_trace_number) {
  if (!weak_escher) {
    // This class is not functional without a valid escher.
    FX_LOGS(WARNING) << "Error, creating a BatchGpuUploader without an escher.";
    return nullptr;
  }
  return std::make_unique<BatchGpuUploader>(std::move(weak_escher), frame_trace_number);
}

BatchGpuUploader::BatchGpuUploader(EscherWeakPtr weak_escher, uint64_t frame_trace_number)
    : escher_(std::move(weak_escher)),
      frame_trace_number_(frame_trace_number),
      buffer_cache_(escher_->buffer_cache()->GetWeakPtr()) {
  FX_DCHECK(escher_);
  FX_DCHECK(buffer_cache_);
}

BatchGpuUploader::~BatchGpuUploader() {
  FX_CHECK(resources_.empty() && copy_info_records_.empty() && wait_semaphores_.empty() &&
           signal_semaphores_.empty() && current_offset_ == 0U);
}

void BatchGpuUploader::ScheduleWriteBuffer(const BufferPtr& target,
                                           DataProviderCallback write_function,
                                           vk::DeviceSize target_offset, vk::DeviceSize copy_size) {
  TRACE_DURATION("gfx", "escher::BatchGpuUploader::ScheduleWriteBuffer");
  vk::DeviceSize src_offset = AlignedToNext(current_offset_, kByteAlignment);

  FX_CHECK(target_offset < target->size() && copy_size > 0);

  vk::DeviceSize writeable_size = target->size() - target_offset;
  FX_DCHECK(writeable_size >= copy_size) << "copy_size + target_offset exceeds the buffer size";
  auto write_size = std::min(copy_size, writeable_size);
  auto region = vk::BufferCopy(src_offset, target_offset, write_size);

  copy_info_records_.push_back(
      CopyInfo{.type = CopyType::COPY_BUFFER,
               .offset = src_offset,
               .size = write_size,
               .write_function = std::move(write_function),
               .copy_info = BufferCopyInfo{.target = target, .region = region}});
  current_offset_ = src_offset + copy_info_records_.back().size;

  // Keep the target alive until Submit().
  resources_.push_back(target);
}

void BatchGpuUploader::ScheduleWriteImage(const ImagePtr& target,
                                          DataProviderCallback write_function,
                                          vk::ImageLayout final_layout,
                                          vk::BufferImageCopy region) {
  ScheduleWriteImageImpl(target, std::move(write_function), final_layout, region,
                         /*available_host_bytes=*/std::nullopt);
}

void BatchGpuUploader::ScheduleWriteImageImpl(const ImagePtr& target,
                                              DataProviderCallback write_function,
                                              vk::ImageLayout final_layout,
                                              vk::BufferImageCopy region,
                                              std::optional<vk::DeviceSize> available_host_bytes) {
  // Create default buffer image copy if the region is empty.
  if (region == vk::BufferImageCopy()) {
    region = impl::GetDefaultBufferImageCopy(target->width(), target->height());
  }

  FX_DCHECK(region.bufferOffset == 0U);

  // An empty extent would size an empty staging range, and an upload whose only record is empty
  // fails to allocate its staging buffer. These checks run before the debug-only whole-image check
  // below so that they fire in every build.
  FX_CHECK(region.imageExtent.width > 0U)
      << "Only non-empty copy regions are supported for upload: imageExtent.width is 0";
  FX_CHECK(region.imageExtent.height > 0U)
      << "Only non-empty copy regions are supported for upload: imageExtent.height is 0";
  FX_CHECK(region.imageExtent.depth == 1U)
      << "Only 2D copy regions are supported for upload: imageExtent.depth is "
      << region.imageExtent.depth;

  // For now we expect that we only accept full image to be uploadable.
  FX_DCHECK(region.imageOffset == vk::Offset3D(0, 0, 0) &&
            region.imageExtent == vk::Extent3D(target->width(), target->height(), 1U));

  if (region.imageOffset.x < 0 || region.imageOffset.y < 0 || region.imageOffset.z < 0 ||
      static_cast<uint64_t>(region.imageOffset.x) + region.imageExtent.width > target->width() ||
      static_cast<uint64_t>(region.imageOffset.y) + region.imageExtent.height > target->height() ||
      static_cast<uint64_t>(region.imageOffset.z) + region.imageExtent.depth > 1U) {
    FX_DCHECK(false);
    return;
  }

  TRACE_DURATION("gfx", "escher::BatchGpuUploader::ScheduleWriteImage");

  // Size the staging range from the copy region rather than from `Image::size()`, which is 0 for
  // images created by `Image::WrapVkImage()` and may include allocation padding. The range must
  // cover exactly the bytes `vkCmdCopyBufferToImage()` reads, so only tightly packed, single-layer
  // color regions of the formats `IsUploadFormatSupported()` accepts are supported.
  // Unsupported formats and layouts fail an `FX_CHECK` in every build rather than dropping the
  // upload. No staging range is reserved yet, so dropping would be memory safe, but it would leave
  // the image contents undefined with nothing logged in release builds. The out-of-bounds check
  // above stays debug-only and drops the upload in release builds.
  const vk::Format format = target->format();
  FX_CHECK(IsUploadFormatSupported(format))
      << "Unsupported image format for upload: " << vk::to_string(format);
  FX_CHECK(region.imageSubresource.aspectMask == vk::ImageAspectFlagBits::eColor)
      << "Only color copy regions are supported for upload";
  FX_CHECK(region.imageSubresource.layerCount == 1U)
      << "Only single-layer copy regions are supported for upload";
  FX_CHECK(region.bufferRowLength == 0U)
      << "Only tightly packed copy regions are supported for upload: bufferRowLength is "
      << region.bufferRowLength;
  FX_CHECK(region.bufferImageHeight == 0U)
      << "Only tightly packed copy regions are supported for upload: bufferImageHeight is "
      << region.bufferImageHeight;
  // `GetImageByteSize()` multiplies by up to 4 bytes per pixel in `size_t`; reject a size that
  // wraps.
  FX_CHECK(uint64_t{region.imageExtent.width} * region.imageExtent.height <=
           std::numeric_limits<size_t>::max() / 4)
      << "Copy region is too large for upload";
  const vk::DeviceSize copy_size =
      image_utils::GetImageByteSize(format, region.imageExtent.width, region.imageExtent.height);
  if (available_host_bytes) {
    FX_CHECK(*available_host_bytes >= copy_size)
        << "host_data has " << *available_host_bytes << " bytes, but the copy region reads "
        << copy_size << " bytes";
  }

  vk::DeviceSize src_offset = AlignedToNext(current_offset_, kByteAlignment);
  auto final_region = region;
  final_region.setBufferOffset(src_offset);

  copy_info_records_.push_back(CopyInfo{
      .type = CopyType::COPY_IMAGE,
      .offset = src_offset,
      .size = copy_size,
      .write_function = std::move(write_function),
      .copy_info =
          ImageCopyInfo{.target = target, .region = final_region, .final_layout = final_layout}});
  current_offset_ = src_offset + copy_info_records_.back().size;

  // Keep the target alive until Submit().
  resources_.push_back(target);
}

BufferPtr BatchGpuUploader::CreateBufferFromRecords() {
  FX_DCHECK(buffer_cache_);
  FX_DCHECK(HasContentToUpload());

  vk::DeviceSize buffer_size = copy_info_records_.back().offset + copy_info_records_.back().size;
  BufferPtr src_buffer = buffer_cache_->NewHostBuffer(buffer_size);
  FX_DCHECK(src_buffer) << "Error allocating buffer";

  for (const auto& copy_info_record : copy_info_records_) {
    auto dst_ptr = src_buffer->host_ptr() + copy_info_record.offset;
    auto copy_size = copy_info_record.size;
    copy_info_record.write_function(dst_ptr, copy_size);
  }

  return src_buffer;
}

void BatchGpuUploader::GenerateCommands(CommandBuffer* cmds) {
  if (!NeedsCommandBuffer()) {
    return;
  }

  TRACE_DURATION("gfx", "BatchGpuUploader::GenerateCommands");
  // Check existence of command buffer and buffer cache.
  FX_DCHECK(cmds);

  // We only create the src_buffer if we need to upload something.
  // If we only need to create an empty command buffer (to signal / wait on
  // semaphores), we won't need the buffer.
  BufferPtr src_buffer = BufferPtr();
  if (HasContentToUpload()) {
    src_buffer = CreateBufferFromRecords();
    cmds->KeepAlive(src_buffer);
  }

  // Set up pipeline flags and access flags for synchronization. See class
  // comments for details.
  constexpr auto kPipelineFlag = vk::PipelineStageFlagBits::eTransfer;
  const auto kAccessFlagOutside =
      vk::AccessFlagBits::eTransferRead | vk::AccessFlagBits::eTransferWrite;
  const auto kAccessFlagInside = vk::AccessFlagBits::eTransferWrite;

  for (const auto& copy_info_record : copy_info_records_) {
    switch (copy_info_record.type) {
      case CopyType::COPY_IMAGE: {
        const auto* image_copy_info = std::get_if<ImageCopyInfo>(&copy_info_record.copy_info);
        FX_DCHECK(image_copy_info);

        ImagePtr target = image_copy_info->target;
        auto final_layout = image_copy_info->final_layout;
        cmds->ImageBarrier(target, target->layout(), vk::ImageLayout::eTransferDstOptimal,
                           kPipelineFlag, kAccessFlagOutside, kPipelineFlag, kAccessFlagInside);
        cmds->vk().copyBufferToImage(src_buffer->vk(), target->vk(),
                                     vk::ImageLayout::eTransferDstOptimal, 1,
                                     &image_copy_info->region);
        cmds->ImageBarrier(target, vk::ImageLayout::eTransferDstOptimal, final_layout,
                           kPipelineFlag, kAccessFlagInside, kPipelineFlag, kAccessFlagOutside);
        cmds->KeepAlive(target);
        break;
      }
      case CopyType::COPY_BUFFER: {
        const auto* buffer_copy_info = std::get_if<BufferCopyInfo>(&copy_info_record.copy_info);
        FX_DCHECK(buffer_copy_info);

        cmds->BufferBarrier(buffer_copy_info->target, kPipelineFlag, kAccessFlagOutside,
                            kPipelineFlag, kAccessFlagInside);
        cmds->vk().copyBuffer(src_buffer->vk(), buffer_copy_info->target->vk(), 1,
                              &buffer_copy_info->region);
        cmds->BufferBarrier(buffer_copy_info->target, kPipelineFlag, kAccessFlagInside,
                            kPipelineFlag, kAccessFlagOutside);

        cmds->KeepAlive(buffer_copy_info->target);
        break;
      }
    }
  }

  // Add semaphores for the submitted command buffer to wait on / signal.
  for (auto& pair : wait_semaphores_) {
    cmds->AddWaitSemaphore(std::move(pair.first), pair.second);
  }
  wait_semaphores_.clear();

  for (auto& sem : signal_semaphores_) {
    cmds->AddSignalSemaphore(std::move(sem));
  }
  signal_semaphores_.clear();

  // Since we keep the target alive using CommandBuffer, we can remove these
  // RefPtrs from |resources_| right now.
  resources_.clear();
  copy_info_records_.clear();
  current_offset_ = 0U;
}

void BatchGpuUploader::Submit(fit::function<void()> callback) {
  if (!NeedsCommandBuffer()) {
    // This uploader was never used, nothing to submit.
    if (callback) {
      callback();
    }
    return;
  }

  TRACE_DURATION("gfx", "BatchGpuUploader::Submit");
  // Create new command buffer.
  auto frame = escher_->NewFrame("Gpu Uploader", frame_trace_number_,
                                 /*enable_gpu_logging=*/false, CommandBuffer::Type::kTransfer,
                                 /*use_protected_memory=*/false);

  // Add commands to |frame|'s command buffer.
  GenerateCommands(frame->cmds());

  // Submit the command buffer.
  frame->EndFrame(SemaphorePtr(), [callback = std::move(callback)]() {
    if (callback) {
      callback();
    }
  });

  // Verify that everything is reset so that the uploader can be used as though
  // it's new.
  FX_CHECK(resources_.empty() && copy_info_records_.empty() && wait_semaphores_.empty() &&
           signal_semaphores_.empty() && current_offset_ == 0U);
}

void BatchGpuUploader::AddWaitSemaphore(SemaphorePtr sema, vk::PipelineStageFlags flags) {
  wait_semaphores_.push_back({std::move(sema), flags});
}

void BatchGpuUploader::AddSignalSemaphore(SemaphorePtr sema) {
  signal_semaphores_.push_back(std::move(sema));
}

}  // namespace escher
