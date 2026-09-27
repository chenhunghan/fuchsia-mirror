// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "src/media/codec/codecs/vaapi/third_party/chromium/vaapi_wrapper.h"

#include <zircon/status.h>

#include <va/va_drmcommon.h>

#include "src/media/codec/codecs/vaapi/third_party/chromium/vaapi_video_encoder_delegate.h"
#include "src/media/codec/codecs/vaapi/vaapi_utils.h"

namespace media {

bool VaapiWrapper::UploadVideoFrameToSurface(const VideoFrame& frame, VASurfaceID input_surface_id,
                                             const gfx::Size& input_surface_size) {
  if (frame.display_size.IsEmpty() || frame.coded_size.IsEmpty() || input_surface_size.IsEmpty()) {
    FX_LOGS(WARNING) << "Invalid empty dimensions: display=" << frame.display_size.ToString()
                     << " coded=" << frame.coded_size.ToString()
                     << " surface=" << input_surface_size.ToString();
    return false;
  }
  if (frame.display_size.width() > frame.coded_size.width() ||
      frame.display_size.height() > frame.coded_size.height()) {
    FX_LOGS(WARNING) << "Display dimensions " << frame.display_size.ToString()
                     << " exceed coded dimensions " << frame.coded_size.ToString();
    return false;
  }
  if (frame.display_size.width() > input_surface_size.width() ||
      frame.display_size.height() > input_surface_size.height()) {
    FX_LOGS(WARNING) << "Display dimensions " << frame.display_size.ToString()
                     << " exceed input surface dimensions " << input_surface_size.ToString();
    return false;
  }
  if (safemath::checked_cast<uint32_t>(frame.coded_size.width()) > frame.stride) {
    FX_LOGS(WARNING) << "Invalid image stride " << frame.coded_size.width() << " vs "
                     << frame.stride;
    return false;
  }
  auto main_plane_size =
      safemath::CheckMul(frame.stride, safemath::checked_cast<uint32_t>(frame.coded_size.height()));
  auto uv_coded_height = CheckedRoundUp(frame.coded_size.height(), 2) / 2;
  auto uv_plane_size = safemath::CheckMul(frame.stride, uv_coded_height.Cast<uint32_t>());
  auto pic_size_checked = main_plane_size + uv_plane_size;
  if (!pic_size_checked.IsValid() || pic_size_checked.ValueOrDie() > frame.size_bytes) {
    FX_LOGS(WARNING) << "Invalid image dimensions stride " << frame.stride << " coded height "
                     << frame.coded_size.height() << " byte size " << frame.size_bytes;
    return false;
  }

  size_t y_rows = safemath::checked_cast<size_t>(frame.display_size.height());
  size_t y_row_bytes = safemath::checked_cast<size_t>(frame.display_size.width());
  auto uv_rows_checked = CheckedRoundUp(frame.display_size.height(), 2) / 2;
  auto uv_row_bytes_checked = CheckedRoundUp(frame.display_size.width(), 2);
  if (!uv_rows_checked.IsValid() || !uv_row_bytes_checked.IsValid()) {
    return false;
  }
  size_t uv_rows = safemath::checked_cast<size_t>(uv_rows_checked.ValueOrDie());
  size_t uv_row_bytes = safemath::checked_cast<size_t>(uv_row_bytes_checked.ValueOrDie());
  if (uv_row_bytes > frame.stride) {
    FX_LOGS(WARNING) << "UV row bytes exceed input frame stride";
    return false;
  }

  VAImage image;
  VAStatus status =
      vaDeriveImage(VADisplayWrapper::GetSingleton()->display(), input_surface_id, &image);
  if (status != VA_STATUS_SUCCESS) {
    FX_LOGS(WARNING) << "DeriveImage failed: " << status;
    return false;
  }
  ScopedImageID scoped_image(image.image_id);

  if (image.num_planes < 2 || frame.display_size.width() > static_cast<int>(image.width) ||
      frame.display_size.height() > static_cast<int>(image.height)) {
    FX_LOGS(WARNING) << "Destination VAImage dimensions or plane count insufficient";
    return false;
  }

  if (y_row_bytes > image.pitches[0] || uv_row_bytes > image.pitches[1]) {
    FX_LOGS(WARNING) << "Row bytes exceed destination VAImage pitch";
    return false;
  }

  auto y_end_offset = safemath::CheckAdd(
      image.offsets[0],
      safemath::CheckAdd(safemath::CheckMul(y_rows - 1, static_cast<size_t>(image.pitches[0])),
                         y_row_bytes));
  auto uv_end_offset = safemath::CheckAdd(
      image.offsets[1],
      safemath::CheckAdd(safemath::CheckMul(uv_rows - 1, static_cast<size_t>(image.pitches[1])),
                         uv_row_bytes));
  if (!y_end_offset.IsValid() || !uv_end_offset.IsValid() ||
      y_end_offset.ValueOrDie() > image.data_size || uv_end_offset.ValueOrDie() > image.data_size) {
    FX_LOGS(WARNING) << "Destination VAImage buffer too small for frame upload";
    return false;
  }

  // In NV12, the Y plane write interval [image.offsets[0], y_end_offset)
  // precedes the UV plane write interval [image.offsets[1], uv_end_offset) in
  // memory. Verify that image.offsets[0] <= image.offsets[1] and that Y plane
  // scanlines do not spill into the start of the UV plane
  // (y_end_offset <= image.offsets[1]).
  if (image.offsets[0] > image.offsets[1] || y_end_offset.ValueOrDie() > image.offsets[1]) {
    FX_LOGS(WARNING) << "Y plane overlaps or follows UV plane in destination VAImage";
    return false;
  }

  void* surface_p;
  status = vaMapBuffer(VADisplayWrapper::GetSingleton()->display(), image.buf, &surface_p);
  if (status != VA_STATUS_SUCCESS) {
    FX_LOGS(WARNING) << "MapBuffer failed: " << status;
    return false;
  }
  // TODO(https://fxbug.dev/42051383): Optimize this code to reduce copies.
  const uint8_t* in_ptr = frame.base;
  uint8_t* out_ptr = static_cast<uint8_t*>(surface_p);
  for (size_t y = 0; y < y_rows; y++) {
    const uint8_t* in_start = in_ptr + y * frame.stride;
    uint8_t* out_start = out_ptr + image.offsets[0] + image.pitches[0] * y;
    memcpy(out_start, in_start, y_row_bytes);
  }

  for (size_t y = 0; y < uv_rows; y++) {
    const uint8_t* in_start =
        in_ptr + (safemath::checked_cast<size_t>(frame.coded_size.height()) + y) * frame.stride;
    uint8_t* out_start = out_ptr + image.offsets[1] + image.pitches[1] * y;
    memcpy(out_start, in_start, uv_row_bytes);
  }
  vaUnmapBuffer(VADisplayWrapper::GetSingleton()->display(), image.buf);

  status = vaDestroyImage(VADisplayWrapper::GetSingleton()->display(), scoped_image.release());
  if (status != VA_STATUS_SUCCESS) {
    FX_LOGS(WARNING) << "DestroyImage failed: " << status;
    return false;
  }
  return true;
}

bool VaapiWrapper::SubmitBuffer(VABufferType va_buffer_type, size_t size, const void* data) {
  VABufferID buffer_id;
  const VAStatus va_res =
      vaCreateBuffer(VADisplayWrapper::GetSingleton()->display(), context_id_, va_buffer_type,
                     static_cast<uint32_t>(size), 1, const_cast<void*>(data), &buffer_id);
  if (va_res != VA_STATUS_SUCCESS) {
    FX_LOGS(ERROR) << "Failed to create buffer " << va_res;
    return false;
  }
  buffer_ids_.emplace_back(buffer_id);

  return true;
}

bool VaapiWrapper::GetSupportedPackedHeaders(media::VideoCodecProfile profile, bool& packed_sps,
                                             bool& packed_pps, bool& packed_slice) {
  VAConfigAttrib attrib{};
  attrib.type = VAConfigAttribEncPackedHeaders;
  constexpr VAProfile kProfile = VAProfileH264High;
  const VAStatus va_res = vaGetConfigAttributes(VADisplayWrapper::GetSingleton()->display(),
                                                kProfile, VAEntrypointEncSliceLP, &attrib, 1);

  if (va_res != VA_STATUS_SUCCESS) {
    FX_LOGS(ERROR) << "Failed to get packed header attrib " << va_res;
    return false;
  }
  packed_sps = attrib.value & VA_ENC_PACKED_HEADER_SEQUENCE;
  packed_pps = attrib.value & VA_ENC_PACKED_HEADER_PICTURE;
  packed_slice = attrib.value & VA_ENC_PACKED_HEADER_SLICE;
  return true;
}

bool VaapiWrapper::ExecuteAndDestroyPendingBuffers(VASurfaceID surface_id) {
  VADisplay va_display = VADisplayWrapper::GetSingleton()->display();
  VAStatus va_res = vaBeginPicture(va_display, context_id_, surface_id);
  if (va_res != VA_STATUS_SUCCESS) {
    FX_LOGS(ERROR) << "Failed to begin picture " << va_res;
    return false;
  }

  if (!buffer_ids_.empty()) {
    std::vector<VABufferID> buffer_ids;
    for (auto& id : buffer_ids_) {
      buffer_ids.push_back(id.id());
    }
    va_res = vaRenderPicture(va_display, context_id_, buffer_ids.data(),
                             base::checked_cast<int>(buffer_ids.size()));
    if (va_res != VA_STATUS_SUCCESS) {
      FX_LOGS(ERROR) << "Failed to render picture " << va_res;
      return false;
    }
  }

  // Instruct HW codec to start processing the submitted commands. In theory,
  // this shouldn't be blocking, relying on vaSyncSurface() instead, however
  // evidence points to it actually waiting for the job to be done.
  va_res = vaEndPicture(va_display, context_id_);
  if (va_res != VA_STATUS_SUCCESS) {
    FX_LOGS(ERROR) << "Failed to end picture " << va_res;
    return false;
  }

  buffer_ids_.clear();

  return true;
}

uint64_t VaapiWrapper::GetEncodedChunkSize(VABufferID buffer_id, VASurfaceID surface_id) {
  VADisplay va_display = VADisplayWrapper::GetSingleton()->display();
  void* va_buffer_data;
  VAStatus va_res = vaSyncSurface(va_display, surface_id);
  if (va_res != VA_STATUS_SUCCESS) {
    FX_LOGS(ERROR) << "Failed to sync surface " << va_res;
    return 0;
  }

  // On Intel MapBuffer does a sync.
  va_res = vaMapBuffer(va_display, buffer_id, &va_buffer_data);
  if (va_res != VA_STATUS_SUCCESS) {
    FX_LOGS(ERROR) << "Failed to map buffer " << va_res;
    return 0;
  }
  uint64_t coded_data_size = 0;
  for (auto* buffer_segment = reinterpret_cast<VACodedBufferSegment*>(va_buffer_data);
       buffer_segment;
       buffer_segment = reinterpret_cast<VACodedBufferSegment*>(buffer_segment->next)) {
    coded_data_size += buffer_segment->size;
  }
  vaUnmapBuffer(va_display, buffer_id);
  return coded_data_size;
}

}  // namespace media
