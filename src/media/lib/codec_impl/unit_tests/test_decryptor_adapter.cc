// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.media.drm/cpp/fidl.h>
#include <fidl/fuchsia.media/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/executor.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <random>
#include <set>
#include <tuple>
#include <unordered_map>
#include <variant>
#include <vector>

#include <gtest/gtest.h>

#include "lib/media/codec_impl/codec_impl.h"
#include "lib/media/codec_impl/decryptor_adapter.h"
#include "src/lib/fsl/handles/object_info.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace {

constexpr uint64_t kBufferConstraintsVersionOrdinal = 1;
constexpr uint64_t kBufferLifetimeOrdinal = 1;
constexpr uint64_t kStreamLifetimeOrdinal = 1;
constexpr uint32_t kInputPacketSize = 8 * 1024;

auto CreateDecryptorParams(bool is_secure, std::optional<std::string> mime_type = std::nullopt) {
  fuchsia_media_drm::DecryptorParams params;
  auto& input_details = params.input_details().emplace();
  input_details.format_details_version_ordinal() = 0;
  if (mime_type.has_value()) {
    input_details.mime_type() = std::move(*mime_type);
  }
  if (is_secure) {
    params.require_secure_mode() = is_secure;
  }
  return params;
}

auto CreateStreamBufferPartialSettings(
    const fuchsia_media::StreamBufferConstraints& constraints,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
  fuchsia_media::StreamBufferPartialSettings settings;
  settings.buffer_lifetime_ordinal() = kBufferLifetimeOrdinal;
  settings.buffer_constraints_version_ordinal() = kBufferConstraintsVersionOrdinal;
  settings.sysmem2_token() = std::move(token);
  return settings;
}

auto CreateBufferCollectionConstraints(uint32_t cpu_usage) {
  fuchsia_sysmem2::BufferCollectionConstraints collection_constraints;

  collection_constraints.usage().emplace().cpu() = cpu_usage;
  collection_constraints.min_buffer_count_for_camping() = 1;
  collection_constraints.buffer_memory_constraints().emplace().min_size_bytes() = kInputPacketSize;

  // Secure buffers not allowed for test keys
  EXPECT_FALSE(collection_constraints.buffer_memory_constraints()->secure_required().has_value());

  return collection_constraints;
}

auto CreateInputFormatDetails(const std::string& scheme, const std::vector<uint8_t>& key_id,
                              const std::vector<uint8_t>& init_vector) {
  constexpr uint64_t kFormatDetailsVersionOrdinal = 1;

  fuchsia_media::FormatDetails details;
  details.format_details_version_ordinal() = kFormatDetailsVersionOrdinal;
  auto& encrypted_format = details.encrypted_format().emplace();
  encrypted_format.scheme() = scheme;
  encrypted_format.key_id() = key_id;
  encrypted_format.init_vector() = init_vector;

  return details;
}

class FakeDecryptorAdapter : public DecryptorAdapter {
 public:
  explicit FakeDecryptorAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events,
                                inspect::Node node)
      : DecryptorAdapter(lock, codec_adapter_events, std::move(node)) {}

  void set_has_keys(bool has_keys) { has_keys_ = has_keys; }
  bool has_keys() const { return has_keys_; }

  void set_use_mapped_output(bool use_mapped_output) { use_mapped_output_ = use_mapped_output; }
  bool use_mapped_output() const { return use_mapped_output_; }

  void set_output_mime_type(std::string mime_type) { output_mime_type_ = std::move(mime_type); }

  fuchsia_media::StreamOutputFormat CoreCodecGetOutputFormat2(
      uint64_t stream_lifetime_ordinal,
      uint64_t new_output_format_details_version_ordinal) override {
    auto format = DecryptorAdapter::CoreCodecGetOutputFormat2(
        stream_lifetime_ordinal, new_output_format_details_version_ordinal);
    if (output_mime_type_.has_value()) {
      format.format_details()->mime_type() = *output_mime_type_;
    }
    return format;
  }

  bool IsCoreCodecMappedBufferUseful(CodecPort port) override {
    if (!use_mapped_output_ && port == kOutputPort) {
      return false;
    } else {
      return DecryptorAdapter::IsCoreCodecMappedBufferUseful(port);
    }
  }

 private:
  bool has_keys_ = false;
  bool use_mapped_output_ = true;
  std::optional<std::string> output_mime_type_;
};

class ClearTextDecryptorAdapter : public FakeDecryptorAdapter {
 public:
  explicit ClearTextDecryptorAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events,
                                     inspect::Node node)
      : FakeDecryptorAdapter(lock, codec_adapter_events, std::move(node)) {}

  std::optional<fuchsia_media::StreamError> Decrypt(const EncryptionParams2& params,
                                                    const InputBuffer& input,
                                                    const OutputBuffer& output,
                                                    CodecPacket* output_packet) override {
    if (!std::holds_alternative<ClearOutputBuffer>(output)) {
      fprintf(stderr, "!std::holds_alternative<ClearOutputBuffer>(output)\n");
      return fuchsia_media::StreamError::kDecryptorUnknown;
    }
    auto& clear_output = std::get<ClearOutputBuffer>(output);

    if (input.data_length > clear_output.data_length) {
      fprintf(stderr, "input.data_length > clear_output.data_length -- input: %u output: %u\n",
              input.data_length, clear_output.data_length);
      return fuchsia_media::StreamError::kDecryptorUnknown;
    }

    if (!has_keys()) {
      fprintf(stderr, "!has_keys()\n");
      return fuchsia_media::StreamError::kDecryptorNoKey;
    }

    std::memcpy(clear_output.data, input.data, input.data_length);

    return std::nullopt;
  }
};

class FakeSecureDecryptorAdapter : public FakeDecryptorAdapter {
 public:
  explicit FakeSecureDecryptorAdapter(std::mutex& lock, CodecAdapterEvents* codec_adapter_events,
                                      inspect::Node node)
      : FakeDecryptorAdapter(lock, codec_adapter_events, std::move(node)) {}

  std::optional<fuchsia_media::StreamError> Decrypt(const EncryptionParams2& params,
                                                    const InputBuffer& input,
                                                    const OutputBuffer& output,
                                                    CodecPacket* output_packet) override {
    // We should not get here, so just return an error.
    return fuchsia_media::StreamError::kDecryptorUnknown;
  }
};

}  // namespace

template <typename DecryptorAdapterT>
class DecryptorAdapterTest : public gtest::RealLoopFixture {
 protected:
  class AllocatorEventHandler : public fidl::AsyncEventHandler<fuchsia_sysmem2::Allocator> {
   public:
    explicit AllocatorEventHandler(std::optional<zx_status_t>* error) : error_(error) {}
    void on_fidl_error(fidl::UnbindInfo info) override { *error_ = info.status(); }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_sysmem2::Allocator> metadata) override {}

   private:
    std::optional<zx_status_t>* error_;
  };

  class CollectionEventHandler : public fidl::AsyncEventHandler<fuchsia_sysmem2::BufferCollection> {
   public:
    explicit CollectionEventHandler(std::optional<zx_status_t>* error) : error_(error) {}
    void on_fidl_error(fidl::UnbindInfo info) override { *error_ = info.status(); }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_sysmem2::BufferCollection> metadata) override {}

   private:
    std::optional<zx_status_t>* error_;
  };

  class StreamProcessorEventHandler
      : public fidl::AsyncEventHandler<fuchsia_media::StreamProcessor> {
   public:
    explicit StreamProcessorEventHandler(DecryptorAdapterTest* parent) : parent_(parent) {}
    void on_fidl_error(fidl::UnbindInfo info) override {
      parent_->decryptor_error_ = info.status();
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_media::StreamProcessor> metadata) override {}
    void OnStreamFailed(
        fidl::Event<fuchsia_media::StreamProcessor::OnStreamFailed>& event) override {
      parent_->OnStreamFailed(event.stream_lifetime_ordinal(), event.error());
    }
    void OnInputConstraints(
        fidl::Event<fuchsia_media::StreamProcessor::OnInputConstraints>& event) override {
      parent_->OnInputConstraints(std::move(event.input_constraints()));
    }
    void OnOutputConstraints(
        fidl::Event<fuchsia_media::StreamProcessor::OnOutputConstraints>& event) override {
      parent_->OnOutputConstraints(std::move(event.output_config()));
    }
    void OnOutputFormat(
        fidl::Event<fuchsia_media::StreamProcessor::OnOutputFormat>& event) override {
      parent_->OnOutputFormat(std::move(event.output_format()));
    }
    void OnOutputPacket(
        fidl::Event<fuchsia_media::StreamProcessor::OnOutputPacket>& event) override {
      parent_->OnOutputPacket(std::move(event.output_packet()), event.error_detected_before(),
                              event.error_detected_during());
    }
    void OnFreeInputPacket(
        fidl::Event<fuchsia_media::StreamProcessor::OnFreeInputPacket>& event) override {
      parent_->OnFreeInputPacket(std::move(event.free_input_packet()));
    }
    void OnOutputEndOfStream(
        fidl::Event<fuchsia_media::StreamProcessor::OnOutputEndOfStream>& event) override {
      parent_->OnOutputEndOfStream(event.stream_lifetime_ordinal(), event.error_detected_before());
    }
    void OnOutputTimestampHasNoOutput(
        fidl::Event<fuchsia_media::StreamProcessor::OnOutputTimestampHasNoOutput>& event) override {
    }

   private:
    DecryptorAdapterTest* parent_;
  };

  DecryptorAdapterTest()
      : allocator_handler_(&sysmem_error_),
        input_collection_handler_(&input_collection_error_),
        output_collection_handler_(&output_collection_error_),
        decryptor_handler_(this),
        random_device_(),
        prng_(random_device_()) {
    auto allocator_end = component::Connect<fuchsia_sysmem2::Allocator>();
    ZX_ASSERT(allocator_end.is_ok());
    allocator_.Bind(std::move(*allocator_end), dispatcher(), &allocator_handler_);
    fuchsia_sysmem2::AllocatorSetDebugClientInfoRequest debug_info;
    debug_info.name() = fsl::GetCurrentProcessName();
    debug_info.id() = fsl::GetCurrentProcessKoid();
    std::ignore = allocator_->SetDebugClientInfo(std::move(debug_info));

    PopulateInputData();
  }

  ~DecryptorAdapterTest() {
    // Cleanly terminate BufferCollection view to avoid errors as the test halts.
    if (input_collection_.is_valid()) {
      std::ignore = input_collection_->Release();
    }
    if (output_collection_.is_valid()) {
      std::ignore = output_collection_->Release();
    }
  }

  void ConnectDecryptor(bool is_secure, std::optional<std::string> mime_type = std::nullopt) {
    auto allocator_result = component::Connect<fuchsia_sysmem2::Allocator>();
    ZX_ASSERT(allocator_result.is_ok());
    auto allocator = std::move(allocator_result.value());
    auto endpoints = fidl::Endpoints<fuchsia_media::StreamProcessor>::Create();
    decryptor_.Bind(std::move(endpoints.client), dispatcher(), &decryptor_handler_);
    codec_impl_ = std::make_unique<CodecImpl>(
        std::move(allocator), nullptr, dispatcher(),
        CreateDecryptorParams(is_secure, std::move(mime_type)), std::move(endpoints.server));
    auto adapter = std::make_unique<DecryptorAdapterT>(
        codec_impl_->lock(), codec_impl_.get(), inspector_.GetRoot().CreateChild("decryptor"));
    // Grab a non-owning reference to the adapter for test manipulation.
    decryptor_adapter_ = adapter.get();
    codec_impl_->SetCoreCodecAdapter(std::move(adapter));

    codec_impl_->BindAsync([this]() { codec_impl_ = nullptr; });
  }

  void OnStreamFailed(uint64_t stream_lifetime_ordinal, fuchsia_media::StreamError error) {
    stream_error_ = error;
    for (const auto& [packet_index, buffer_index] : used_packets_) {
      failed_packets_.insert(packet_index);
    }
  }

  void OnInputConstraints(fuchsia_media::StreamBufferConstraints ic) {
    auto settings = BindBufferCollection(
        input_collection_, &input_collection_handler_,
        fuchsia_sysmem2::kCpuUsageWrite | fuchsia_sysmem2::kCpuUsageWriteOften, ic);
    input_collection_->WaitForAllBuffersAllocated().Then(
        [this](
            fidl::Result<fuchsia_sysmem2::BufferCollection::WaitForAllBuffersAllocated>& result) {
          ASSERT_TRUE(result.is_ok());
          input_buffer_info_ = std::move(result->buffer_collection_info().value());
        });

    input_collection_->Sync().Then(
        [this, settings = std::move(settings)](
            fidl::Result<fuchsia_sysmem2::BufferCollection::Sync>& sync_result) mutable {
          if (!sync_result.is_ok()) {
            return;
          }
          std::ignore = decryptor_->SetInputBufferPartialSettings({std::move(settings)});
        });

    input_constraints_ = std::move(ic);
  }

  void OnOutputConstraints(fuchsia_media::StreamOutputConstraints oc) {
    auto settings =
        BindBufferCollection(output_collection_, &output_collection_handler_,
                             fuchsia_sysmem2::kCpuUsageRead | fuchsia_sysmem2::kCpuUsageReadOften,
                             oc.buffer_constraints().value());
    output_collection_->WaitForAllBuffersAllocated().Then(
        [this](
            fidl::Result<fuchsia_sysmem2::BufferCollection::WaitForAllBuffersAllocated>& result) {
          if (result.is_ok()) {
            output_buffer_info_ = std::move(result->buffer_collection_info().value());
          }
        });

    output_collection_->Sync().Then(
        [this, settings = std::move(settings)](
            fidl::Result<fuchsia_sysmem2::BufferCollection::Sync>& sync_result) mutable {
          if (!sync_result.is_ok()) {
            return;
          }
          std::ignore = decryptor_->SetOutputBufferPartialSettings({std::move(settings)});
          std::ignore = decryptor_->CompleteOutputBufferPartialSettings({kBufferLifetimeOrdinal});
        });

    output_constraints_ = std::move(oc);
  }

  void OnOutputFormat(fuchsia_media::StreamOutputFormat of) { output_format_ = std::move(of); }

  void OnOutputPacket(fuchsia_media::Packet packet, bool error_before, bool error_during) {
    EXPECT_FALSE(error_before);
    EXPECT_FALSE(error_during);
    auto header = packet.header().value();
    output_data_.emplace_back(ExtractPayloadData(std::move(packet)));
    std::ignore = decryptor_->RecycleOutputPacket({std::move(header)});
  }

  void OnFreeInputPacket(fuchsia_media::PacketHeader header) {
    ASSERT_TRUE(header.packet_index().has_value());
    FreePacket(header.packet_index().value());
    if (stream_error_.has_value()) {
      freed_failed_packets_.insert(header.packet_index().value());
      return;
    }
    successful_packet_count_++;
    if (end_of_stream_set_) {
      return;
    }
    PumpInput();
  }

  void OnOutputEndOfStream(uint64_t stream_lifetime_ordinal, bool error_before) {
    end_of_stream_reached_ = true;
  }

  void PopulateInputData() {
    constexpr size_t kNumInputPackets = 50;
    std::uniform_int_distribution<int> dist(0, std::numeric_limits<uint8_t>::max());

    input_data_.reserve(kNumInputPackets);
    for (size_t i = 0; i < kNumInputPackets; i++) {
      std::vector<uint8_t> v(kInputPacketSize);
      std::generate(v.begin(), v.end(),
                    [this, &dist]() { return static_cast<uint8_t>(dist(prng_)); });
      input_data_.emplace_back(std::move(v));
    }
    input_iter_ = input_data_.begin();
  }

  fuchsia_media::StreamBufferPartialSettings BindBufferCollection(
      fidl::Client<fuchsia_sysmem2::BufferCollection>& collection, CollectionEventHandler* handler,
      uint32_t cpu_usage, const fuchsia_media::StreamBufferConstraints& constraints) {
    auto token_endpoints = fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    fuchsia_sysmem2::AllocatorAllocateSharedCollectionRequest alloc_req;
    alloc_req.token_request() = std::move(token_endpoints.server);
    std::ignore = allocator_->AllocateSharedCollection(std::move(alloc_req));

    auto decryptor_token_endpoints =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    fuchsia_sysmem2::BufferCollectionTokenDuplicateRequest dup_req;
    dup_req.rights_attenuation_mask() = ZX_RIGHT_SAME_RIGHTS;
    dup_req.token_request() = std::move(decryptor_token_endpoints.server);
    std::ignore = fidl::Call(token_endpoints.client)->Duplicate(std::move(dup_req));

    auto collection_endpoints = fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
    fuchsia_sysmem2::AllocatorBindSharedCollectionRequest bind_req;
    bind_req.token() = std::move(token_endpoints.client);
    bind_req.buffer_collection_request() = std::move(collection_endpoints.server);
    std::ignore = allocator_->BindSharedCollection(std::move(bind_req));

    collection.Bind(std::move(collection_endpoints.client), dispatcher(), handler);
    fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_req;
    set_constraints_req.constraints() = CreateBufferCollectionConstraints(cpu_usage);
    std::ignore = collection->SetConstraints(std::move(set_constraints_req));

    return CreateStreamBufferPartialSettings(constraints,
                                             std::move(decryptor_token_endpoints.client));
  }

  fuchsia_media::Packet CreateInputPacket(const std::vector<uint8_t>& data) {
    fuchsia_media::Packet packet;
    static uint64_t timestamp_ish = 42;
    uint32_t packet_index;
    uint32_t buffer_index;
    AllocatePacket(&packet_index, &buffer_index);

    const auto& vmo = input_buffer_info_->buffers().value().at(buffer_index).vmo().value();
    uint64_t offset =
        input_buffer_info_->buffers().value().at(buffer_index).vmo_usable_start().value();
    size_t size = data.size();

    // Since test code, no particular reason to bother with mapping.
    auto status = vmo.write(data.data(), offset, size);

    EXPECT_EQ(status, ZX_OK);

    packet.header().emplace().buffer_lifetime_ordinal() = kBufferLifetimeOrdinal;
    packet.header()->packet_index() = packet_index;
    packet.buffer_index() = buffer_index;
    packet.stream_lifetime_ordinal() = kStreamLifetimeOrdinal;
    packet.start_offset() = 0;
    packet.valid_length_bytes() = static_cast<uint32_t>(size);
    packet.timestamp_ish() = timestamp_ish++;
    packet.start_access_unit() = true;

    return packet;
  }

  std::vector<uint8_t> ExtractPayloadData(fuchsia_media::Packet packet) {
    std::vector<uint8_t> data;

    uint32_t buffer_index = packet.buffer_index().value();
    uint32_t offset = packet.start_offset().value();
    uint32_t size = packet.valid_length_bytes().value();

    EXPECT_TRUE(buffer_index < output_buffer_info_->buffers().value().size());

    const auto& buffer = output_buffer_info_->buffers().value().at(buffer_index);

    data.resize(size);
    auto status = buffer.vmo()->read(data.data(), offset, size);
    EXPECT_EQ(status, ZX_OK);

    return data;
  }

  bool HasFreePackets() { return !free_packets_.empty(); }

  void ConfigureInputPackets() {
    ASSERT_TRUE(input_buffer_info_.has_value());

    auto buffer_count = input_buffer_info_->buffers().value().size();
    std::vector<uint32_t> buffers;
    std::vector<uint32_t> packets;
    for (uint32_t i = 0; i < buffer_count; i++) {
      buffers.emplace_back(i);
      packets.emplace_back(i);
    }
    // Shuffle the packet indexes so that they don't align with the buffer indexes
    std::shuffle(packets.begin(), packets.end(), prng_);

    for (uint32_t i = 0; i < buffer_count; i++) {
      free_packets_.emplace(packets[i], buffers[i]);
    }
  }

  void AllocatePacket(uint32_t* packet_index, uint32_t* buffer_index) {
    ASSERT_TRUE(HasFreePackets());
    ASSERT_TRUE(packet_index);
    ASSERT_TRUE(buffer_index);

    auto node = free_packets_.extract(free_packets_.begin());
    *packet_index = node.key();
    *buffer_index = node.mapped();
    used_packets_.insert(std::move(node));
  }

  void FreePacket(uint32_t packet_index) {
    free_packets_.insert(used_packets_.extract(packet_index));
  }

  void PumpInput() {
    while (input_iter_ != input_data_.end() && HasFreePackets()) {
      std::ignore = decryptor_->QueueInputPacket({CreateInputPacket(*input_iter_)});
      input_iter_++;
    }
    if (input_iter_ == input_data_.end() && !end_of_stream_set_) {
      std::ignore = decryptor_->QueueInputEndOfStream({kStreamLifetimeOrdinal});
      end_of_stream_set_ = true;
    }
  }

  void AssertNoChannelErrors() {
    ASSERT_FALSE(decryptor_error_.has_value()) << "Decryptor error = " << *decryptor_error_;
    ASSERT_FALSE(sysmem_error_.has_value()) << "Sysmem error = " << *sysmem_error_;
    ASSERT_FALSE(input_collection_error_.has_value())
        << "Input BufferCollection error = " << *input_collection_error_;
    ASSERT_FALSE(output_collection_error_.has_value())
        << "Output BufferCollection error = " << *output_collection_error_;
  }

  bool HasChannelErrors() {
    return decryptor_error_.has_value() || sysmem_error_.has_value() ||
           input_collection_error_.has_value() || output_collection_error_.has_value();
  }

  inspect::Inspector inspector_;
  std::optional<zx_status_t> sysmem_error_;
  std::optional<zx_status_t> decryptor_error_;
  std::optional<zx_status_t> input_collection_error_;
  std::optional<zx_status_t> output_collection_error_;

  AllocatorEventHandler allocator_handler_;
  CollectionEventHandler input_collection_handler_;
  CollectionEventHandler output_collection_handler_;
  StreamProcessorEventHandler decryptor_handler_;

  fidl::Client<fuchsia_media::StreamProcessor> decryptor_;
  fidl::Client<fuchsia_sysmem2::Allocator> allocator_;
  std::unique_ptr<CodecImpl> codec_impl_;
  DecryptorAdapterT* decryptor_adapter_;

  using DataSet = std::vector<std::vector<uint8_t>>;
  DataSet input_data_;
  DataSet output_data_;

  std::optional<fuchsia_media::StreamBufferConstraints> input_constraints_;
  std::optional<fuchsia_media::StreamOutputConstraints> output_constraints_;
  std::optional<fuchsia_media::StreamOutputFormat> output_format_;
  bool end_of_stream_set_ = false;
  bool end_of_stream_reached_ = false;
  DataSet::const_iterator input_iter_;

  fidl::Client<fuchsia_sysmem2::BufferCollection> input_collection_;
  fidl::Client<fuchsia_sysmem2::BufferCollection> output_collection_;

  std::optional<fuchsia_sysmem2::BufferCollectionInfo> input_buffer_info_;
  std::optional<fuchsia_sysmem2::BufferCollectionInfo> output_buffer_info_;

  std::optional<fuchsia_media::StreamError> stream_error_;

  using PacketMap = std::unordered_map<uint32_t /*packet_index*/, uint32_t /*buffer_index*/>;

  PacketMap free_packets_;
  PacketMap used_packets_;

  unsigned int successful_packet_count_ = 0;
  std::set<uint32_t> failed_packets_;
  std::set<uint32_t> freed_failed_packets_;

  std::random_device random_device_;
  std::mt19937 prng_;
};

class ClearDecryptorAdapterTest : public DecryptorAdapterTest<ClearTextDecryptorAdapter> {};

TEST_F(ClearDecryptorAdapterTest, ClearTextDecrypt) {
  ConnectDecryptor(false);
  decryptor_adapter_->set_has_keys(true);

  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });

  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();

  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails("clear", {}, {})});

  PumpInput();

  RunLoopUntil([this]() {
    return end_of_stream_reached_ &&
           free_packets_.size() == input_buffer_info_->buffers().value().size();
  });

  AssertNoChannelErrors();

  EXPECT_TRUE(input_constraints_.has_value());
  EXPECT_TRUE(output_constraints_.has_value());
  ASSERT_TRUE(output_format_.has_value());
  ASSERT_TRUE(output_format_->format_details().has_value());
  EXPECT_FALSE(output_format_->format_details()->encrypted_format().has_value());
  ASSERT_TRUE(output_format_->format_details()->domain().has_value());
  ASSERT_EQ(output_format_->format_details()->domain()->Which(),
            fuchsia_media::DomainFormat::Tag::kCrypto);
  EXPECT_EQ(output_format_->format_details()->domain()->crypto()->Which(),
            fuchsia_media::CryptoFormat::Tag::kDecrypted);

  ASSERT_TRUE(end_of_stream_reached_);
  // ClearText decryptor just copies data across
  EXPECT_EQ(output_data_, input_data_);

  // Check that all input packets were returned
  EXPECT_EQ(free_packets_.size(), input_buffer_info_->buffers().value().size());
}

TEST_F(ClearDecryptorAdapterTest, ClearTextDecryptWithMimeTypePreservesDecryptedDomain) {
  ConnectDecryptor(false, "video/h264");
  decryptor_adapter_->set_has_keys(true);
  decryptor_adapter_->set_output_mime_type("video/h264");

  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });
  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();
  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails("clear", {}, {})});
  PumpInput();

  RunLoopUntil([this]() {
    return end_of_stream_reached_ &&
           free_packets_.size() == input_buffer_info_->buffers().value().size();
  });
  AssertNoChannelErrors();

  ASSERT_TRUE(output_format_.has_value());
  ASSERT_TRUE(output_format_->format_details().has_value());
  EXPECT_FALSE(output_format_->format_details()->encrypted_format().has_value());
  ASSERT_TRUE(output_format_->format_details()->domain().has_value());
  ASSERT_EQ(output_format_->format_details()->domain()->Which(),
            fuchsia_media::DomainFormat::Tag::kCrypto);
  EXPECT_EQ(output_format_->format_details()->domain()->crypto()->Which(),
            fuchsia_media::CryptoFormat::Tag::kDecrypted);
}

TEST_F(ClearDecryptorAdapterTest, NoKeys) {
  ConnectDecryptor(false);
  decryptor_adapter_->set_has_keys(false);
  std::ignore = decryptor_->EnableOnStreamFailed();

  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });

  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();

  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails("clear", {}, {})});

  PumpInput();

  RunLoopUntil([this]() {
    return stream_error_.has_value() &&
           free_packets_.size() == input_buffer_info_->buffers().value().size();
  });

  AssertNoChannelErrors();

  ASSERT_TRUE(stream_error_.has_value());
  EXPECT_EQ(*stream_error_, fuchsia_media::StreamError::kDecryptorNoKey);

  // Check that all input packets were returned
  EXPECT_EQ(free_packets_.size(), input_buffer_info_->buffers().value().size());

  // Check that no input packets were successfully completed.
  EXPECT_EQ(successful_packet_count_, 0u);

  // Check that all of the packets that were with the server at the time of the stream failure have
  // been returned.
  EXPECT_GT(failed_packets_.size(), 0u);
  EXPECT_GT(freed_failed_packets_.size(), 0u);
  EXPECT_EQ(failed_packets_, freed_failed_packets_);
}

TEST_F(ClearDecryptorAdapterTest, UnmappedOutputBuffers) {
  ConnectDecryptor(false);
  decryptor_adapter_->set_has_keys(true);
  decryptor_adapter_->set_use_mapped_output(false);

  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });

  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();

  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails("clear", {}, {})});

  PumpInput();

  RunLoopUntil([this]() { return HasChannelErrors(); });

  // The decryptor should have failed (since unmapped buffers are unsupported),
  // and nothing else should have failed.
  EXPECT_TRUE(decryptor_error_.has_value());
  decryptor_error_ = std::nullopt;
  AssertNoChannelErrors();
}

TEST_F(ClearDecryptorAdapterTest, DecryptorClosesBuffersCleanly) {
  ConnectDecryptor(false);
  decryptor_adapter_->set_has_keys(true);

  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });
  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();

  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails("clear", {}, {})});

  // Queue a single input packet.
  ASSERT_TRUE(HasFreePackets());
  std::ignore = decryptor_->QueueInputPacket({CreateInputPacket(*input_iter_++)});

  // Wait until the output collection has been allocated.
  RunLoopUntil([this]() { return output_buffer_info_.has_value(); });
  AssertNoChannelErrors();
  ASSERT_TRUE(output_buffer_info_.has_value());

  // Wait until receive first output packet.
  RunLoopUntil([this]() { return !output_data_.empty(); });
  AssertNoChannelErrors();

  // Drop the decryptor_. This should not cause any buffer collection failures.
  decryptor_ = {};
  AssertNoChannelErrors();

  // If the below fail, the collections have failed.
  std::optional<bool> input_result;
  std::optional<bool> output_result;
  EXPECT_TRUE(input_collection_.is_valid());
  EXPECT_TRUE(output_collection_.is_valid());
  input_collection_->CheckAllBuffersAllocated().Then(
      [&input_result](
          fidl::Result<fuchsia_sysmem2::BufferCollection::CheckAllBuffersAllocated>& result) {
        input_result = result.is_ok();
      });
  output_collection_->CheckAllBuffersAllocated().Then(
      [&output_result](
          fidl::Result<fuchsia_sysmem2::BufferCollection::CheckAllBuffersAllocated>& result) {
        output_result = result.is_ok();
      });
  RunLoopUntil([&]() { return input_result.has_value() && output_result.has_value(); });
  EXPECT_TRUE(*input_result);
  EXPECT_TRUE(*output_result);
  AssertNoChannelErrors();
  EXPECT_FALSE(stream_error_.has_value());

  // Buffers are A-OK after dropping decryptor_.

  // ClearText decryptor just copies data across, and only one input packet was sent.
  EXPECT_EQ(output_data_[0], input_data_[0]);
}

TEST_F(ClearDecryptorAdapterTest, InspectValues) {
  ConnectDecryptor(false);
  decryptor_adapter_->set_has_keys(true);
  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });
  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();

  const std::string kScheme = "clear";
  const std::vector<uint8_t> kKeyId = {0xde, 0xad, 0xbe, 0xef};
  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails(kScheme, kKeyId, {})});

  // Queue a single input packet to trigger output buffer allocation.
  ASSERT_TRUE(HasFreePackets());
  std::ignore = decryptor_->QueueInputPacket({CreateInputPacket(*input_iter_++)});

  // Wait until output packet received to ensure the input format details have definitely been
  // processed
  RunLoopUntil([this]() { return !output_data_.empty(); });
  AssertNoChannelErrors();

  async::Executor executor(dispatcher());
  fpromise::result<inspect::Hierarchy> hierarchy;
  executor.schedule_task(inspect::ReadFromInspector(inspector_)
                             .then([&](fpromise::result<inspect::Hierarchy>& result) {
                               hierarchy = std::move(result);
                             }));
  RunLoopUntil([&]() { return !hierarchy.is_pending(); });
  ASSERT_TRUE(hierarchy.is_ok());

  auto* decryptor_hierarchy = hierarchy.value().GetByPath({"decryptor"});
  ASSERT_TRUE(decryptor_hierarchy);

  auto* secure_property =
      decryptor_hierarchy->node().get_property<inspect::BoolPropertyValue>("secure_mode");
  ASSERT_TRUE(secure_property);
  EXPECT_FALSE(secure_property->value());

  auto* scheme_property =
      decryptor_hierarchy->node().get_property<inspect::StringPropertyValue>("scheme");
  ASSERT_TRUE(scheme_property);
  EXPECT_EQ(scheme_property->value(), kScheme);

  auto* key_id_property =
      decryptor_hierarchy->node().get_property<inspect::ByteVectorPropertyValue>("key_id");
  ASSERT_TRUE(key_id_property);
  EXPECT_EQ(key_id_property->value(), kKeyId);

  auto port_validator = [&](const std::string& port) {
    auto* port_hierarchy = decryptor_hierarchy->GetByPath({port});
    ASSERT_TRUE(port_hierarchy);
    auto* buffer_count_property =
        port_hierarchy->node().get_property<inspect::UintPropertyValue>("buffer_count");
    ASSERT_TRUE(buffer_count_property);
    EXPECT_GT(buffer_count_property->value(), 0u);
    auto* packet_count_property =
        port_hierarchy->node().get_property<inspect::UintPropertyValue>("packet_count");
    ASSERT_TRUE(packet_count_property);
    EXPECT_GT(packet_count_property->value(), 0u);
  };

  port_validator("input_port");
  port_validator("output_port");
}

class SecureDecryptorAdapterTest : public DecryptorAdapterTest<FakeSecureDecryptorAdapter> {};

TEST_F(SecureDecryptorAdapterTest, FailsToAcquireSecureBuffers) {
  ConnectDecryptor(true);

  RunLoopUntil([this]() { return input_buffer_info_.has_value(); });

  AssertNoChannelErrors();
  ASSERT_TRUE(input_buffer_info_.has_value());

  ConfigureInputPackets();

  std::ignore = decryptor_->QueueInputFormatDetails(
      {kStreamLifetimeOrdinal, CreateInputFormatDetails("secure", {}, {})});

  PumpInput();

  // TODO(https://fxbug.dev/42086427): Once there is a Sysmem fake that allows us to control
  // behavior, we could force it to give us back "secure" buffers that aren't really secure and go
  // through more of the flow.
  RunLoopUntil(
      [this]() { return decryptor_error_.has_value() && output_collection_error_.has_value(); });

  EXPECT_TRUE(decryptor_error_.has_value());
  EXPECT_TRUE(output_collection_error_.has_value());

  EXPECT_TRUE(input_constraints_.has_value());
  EXPECT_TRUE(output_constraints_.has_value());
  EXPECT_FALSE(output_format_.has_value());

  EXPECT_FALSE(end_of_stream_reached_);
}
