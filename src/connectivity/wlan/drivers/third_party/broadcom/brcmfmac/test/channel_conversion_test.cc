/*
 * Copyright (c) 2019 The Fuchsia Authors
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
 * SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
 * OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
 * CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <fidl/fuchsia.wlan.ieee80211/cpp/wire.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>

#include <array>

#include <gtest/gtest.h>

#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_d11.h"
#include "third_party/bcmdhd/crossdriver/bcmwifi_channels.h"
#include "third_party/bcmdhd/crossdriver/wl_cfg80211.h"

namespace {

class ChannelConversion : public ::testing::Test {
 protected:
  fdf_testing::ScopedGlobalLogger logger_{FUCHSIA_LOG_ERROR};
};

static void verify_channel_to_chanspec(const fuchsia_wlan_ieee80211::wire::ChannelNumber& in_ch,
                                       fuchsia_wlan_ieee80211::wire::ChannelBandwidth cbw) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11_inf), ZX_OK);
  const auto chanspec_result = channel_to_chanspec(
      &d11_inf, in_ch.number, static_cast<fuchsia_wlan_ieee80211::WlanBand>(in_ch.band),
      static_cast<fuchsia_wlan_ieee80211::ChannelBandwidth>(cbw));
  ASSERT_TRUE(chanspec_result.is_ok());
  uint16_t chanspec = chanspec_result.value();

  const auto actual = chanspec_to_channel(&d11_inf, chanspec);
  ASSERT_TRUE(actual.is_ok());

  EXPECT_EQ(actual->primary.number, in_ch.number);
  EXPECT_EQ(actual->primary.band, in_ch.band);
  EXPECT_EQ(actual->cbw, cbw);
}

TEST_F(ChannelConversion, ChannelToChanspec) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  {
    // Try a simple 20 MHz channel in the 2.4 GHz band
    fuchsia_wlan_ieee80211::wire::ChannelNumber in_ch = {.band = WlanBand::kTwoGhz, .number = 11};
    verify_channel_to_chanspec(in_ch, ChannelBandwidth::kCbw20);
  }

  {
    // Try a 40+ MHz channel in the 5 GHz band (primary 44)
    fuchsia_wlan_ieee80211::wire::ChannelNumber in_ch = {.band = WlanBand::kFiveGhz, .number = 44};
    verify_channel_to_chanspec(in_ch, ChannelBandwidth::kCbw40);
  }

  {
    // Try a 40- MHz channel in the 5 GHz band (primary 112)
    fuchsia_wlan_ieee80211::wire::ChannelNumber in_ch = {.band = WlanBand::kFiveGhz, .number = 112};
    verify_channel_to_chanspec(in_ch, ChannelBandwidth::kCbw40Below);
  }
}

static void verify_chanspec_decode(
    chanspec_t chanspec, const fuchsia_wlan_ieee80211::wire::ChannelNumber& expected_primary,
    fuchsia_wlan_ieee80211::wire::ChannelBandwidth expected_cbw) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11_inf), ZX_OK);

  const auto ch = chanspec_to_channel(&d11_inf, chanspec);
  ASSERT_TRUE(ch.is_ok());

  EXPECT_EQ(ch->primary.number, expected_primary.number);
  EXPECT_EQ(ch->primary.band, expected_primary.band);
  EXPECT_EQ(ch->cbw, expected_cbw);
}

TEST_F(ChannelConversion, ChanspecDecode) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  {
    // Try a simple 20 MHz channel in the 2.4 GHz band
    chanspec_t chanspec = CH20MHZ_CHSPEC(11);
    fuchsia_wlan_ieee80211::wire::ChannelNumber expected_primary = {.band = WlanBand::kTwoGhz,
                                                                    .number = 11};
    verify_chanspec_decode(chanspec, expected_primary, ChannelBandwidth::kCbw20);
  }

  {
    // Try a 40+ MHz channel in the 5 GHz band (center 46, SB Upper => control 48)
    chanspec_t chanspec = CH40MHZ_CHSPEC(46, WL_CHANSPEC_CTL_SB_U);
    fuchsia_wlan_ieee80211::wire::ChannelNumber expected_primary = {.band = WlanBand::kFiveGhz,
                                                                    .number = 48};
    verify_chanspec_decode(chanspec, expected_primary, ChannelBandwidth::kCbw40Below);
  }

  {
    // Try a 40- MHz channel in the 5 GHz band (center 110, SB Lower => control 108)
    chanspec_t chanspec = CH40MHZ_CHSPEC(110, WL_CHANSPEC_CTL_SB_L);
    fuchsia_wlan_ieee80211::wire::ChannelNumber expected_primary = {.band = WlanBand::kFiveGhz,
                                                                    .number = 108};
    verify_chanspec_decode(chanspec, expected_primary, ChannelBandwidth::kCbw40);
  }
}

TEST_F(ChannelConversion, Override80P80) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;

  const auto out_cbw =
      enforce_bandwidth_limitations(36, WlanBand::kFiveGhz, ChannelBandwidth::kCbw80P80);
  // Override should only change the bandwidth.
  EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
}

TEST_F(ChannelConversion, Override80P80IgnoresOtherBandwidths) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;
  const std::array<ChannelBandwidth, 4> bandwidths{
      ChannelBandwidth::kCbw20, ChannelBandwidth::kCbw40, ChannelBandwidth::kCbw80,
      ChannelBandwidth::kCbw160};
  for (const auto& bandwidth : bandwidths) {
    const auto out_cbw = enforce_bandwidth_limitations(36, WlanBand::kFiveGhz, bandwidth);
    EXPECT_EQ(out_cbw, bandwidth);
  }
}

TEST_F(ChannelConversion, OverrideWideBandwidthForChannel165) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;
  const std::array<ChannelBandwidth, 2> bandwidths{ChannelBandwidth::kCbw40,
                                                   ChannelBandwidth::kCbw80};

  for (const auto& bandwidth : bandwidths) {
    const auto out_cbw = enforce_bandwidth_limitations(165, WlanBand::kFiveGhz, bandwidth);
    EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
  }
}

TEST_F(ChannelConversion, OverrideWideBandwidthForChannel173) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;
  const auto out_cbw =
      enforce_bandwidth_limitations(173, WlanBand::kFiveGhz, ChannelBandwidth::kCbw40);
  EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
}

TEST_F(ChannelConversion, Override2GhzBandwidth) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;

  const std::array<ChannelBandwidth, 5> bandwidths{
      ChannelBandwidth::kCbw20, ChannelBandwidth::kCbw40, ChannelBandwidth::kCbw40Below,
      ChannelBandwidth::kCbw80, ChannelBandwidth::kCbw160};

  for (const auto& bandwidth : bandwidths) {
    for (uint8_t ch = 1; ch <= 14; ++ch) {
      const auto out_cbw = enforce_bandwidth_limitations(ch, WlanBand::kTwoGhz, bandwidth);
      EXPECT_EQ(out_cbw, ChannelBandwidth::kCbw20);
    }
  }
}

static void verify_round_trip(const fuchsia_wlan_ieee80211::wire::ChannelNumber& in_channel,
                              fuchsia_wlan_ieee80211::wire::ChannelBandwidth in_cbw,
                              fuchsia_wlan_ieee80211::wire::ChannelBandwidth expected_cbw) {
  brcmu_d11inf d11_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11_inf), ZX_OK);

  const auto chanspec_result = channel_to_chanspec(
      &d11_inf, in_channel.number, static_cast<fuchsia_wlan_ieee80211::WlanBand>(in_channel.band),
      static_cast<fuchsia_wlan_ieee80211::ChannelBandwidth>(in_cbw));
  ASSERT_TRUE(chanspec_result.is_ok());
  uint16_t chanspec = chanspec_result.value();
  const auto ch = chanspec_to_channel(&d11_inf, chanspec);
  ASSERT_TRUE(ch.is_ok());

  EXPECT_EQ(ch->primary.number, in_channel.number)
      << "Channel number mismatch for channel " << static_cast<int>(in_channel.number);
  EXPECT_EQ(ch->primary.band, in_channel.band)
      << "Band mismatch for channel " << static_cast<int>(in_channel.number);
  EXPECT_EQ(ch->cbw, expected_cbw)
      << "Bandwidth mismatch for channel " << static_cast<int>(in_channel.number);
}

TEST_F(ChannelConversion, RoundTrip40MHz) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  // 2.4 GHz 40+ MHz (Cbw40) - 2.4 GHz bandwidth limitations enforce 20 MHz width.
  // IEEE Std 802.11-2024 Table E-4 Operating Class 83
  for (uint8_t ch = 1; ch <= 9; ++ch) {
    verify_round_trip({.band = WlanBand::kTwoGhz, .number = ch}, ChannelBandwidth::kCbw40,
                      ChannelBandwidth::kCbw20);
  }

  // 2.4 GHz 40- MHz (Cbw40Below) - 2.4 GHz bandwidth limitations enforce 20 MHz width.
  // IEEE Std 802.11-2024 Table E-4 Operating Class 84
  for (uint8_t ch = 5; ch <= 13; ++ch) {
    verify_round_trip({.band = WlanBand::kTwoGhz, .number = ch}, ChannelBandwidth::kCbw40Below,
                      ChannelBandwidth::kCbw20);
  }

  // 5 GHz 40+ MHz (Cbw40)
  // IEEE Std 802.11-2024 Table E-4 Operating Class 116, 119, 122, 126
  const std::array<uint8_t, 12> five_ghz_40m_plus_channels = {36,  44,  52,  60,  100, 108,
                                                              116, 124, 132, 140, 149, 157};
  for (uint8_t ch : five_ghz_40m_plus_channels) {
    verify_round_trip({.band = WlanBand::kFiveGhz, .number = ch}, ChannelBandwidth::kCbw40,
                      ChannelBandwidth::kCbw40);
  }
  // Channels >= 165 are overridden to 20 MHz.
  for (uint8_t ch : std::initializer_list<uint8_t>{165, 173}) {
    verify_round_trip({.band = WlanBand::kFiveGhz, .number = ch}, ChannelBandwidth::kCbw40,
                      ChannelBandwidth::kCbw20);
  }

  // 5 GHz 40- MHz (Cbw40Below)
  // IEEE Std 802.11-2024 Table E-4 Operating Class 117, 120, 123, 127
  const std::array<uint8_t, 12> five_ghz_40m_minus_channels = {40,  48,  56,  64,  104, 112,
                                                               120, 128, 136, 144, 153, 161};
  for (uint8_t ch : five_ghz_40m_minus_channels) {
    verify_round_trip({.band = WlanBand::kFiveGhz, .number = ch}, ChannelBandwidth::kCbw40Below,
                      ChannelBandwidth::kCbw40Below);
  }
  // Channels >= 165 are overridden to 20 MHz.
  for (uint8_t ch : std::initializer_list<uint8_t>{169, 177}) {
    verify_round_trip({.band = WlanBand::kFiveGhz, .number = ch}, ChannelBandwidth::kCbw40Below,
                      ChannelBandwidth::kCbw20);
  }
}

TEST_F(ChannelConversion, ChanspecD11acToD11nSuccess) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;
  brcmu_d11inf d11n_inf = {.io_type = BRCMU_D11N_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11n_inf), ZX_OK);

  auto verify_d11ac_to_d11n = [&](uint8_t ctl_ch, uint32_t bw, uint8_t expected_center_ch,
                                  WlanBand expected_band, ChannelBandwidth expected_cbw) {
    chanspec_t d11ac_chanspec = 0;
    ASSERT_EQ(channel2chspec(ctl_ch, bw, &d11ac_chanspec), ZX_OK);

    chanspec_t d11n_chanspec = 0;
    ASSERT_EQ(chanspec_d11ac_to_d11n(d11ac_chanspec, &d11n_chanspec), ZX_OK);

    brcmu_chan decoded_d11n = {.chspec = d11n_chanspec};
    EXPECT_EQ(d11n_inf.decchspec(&decoded_d11n), ZX_OK);
    EXPECT_EQ(decoded_d11n.primary.number, ctl_ch);
    EXPECT_EQ(decoded_d11n.primary.band, expected_band);
    EXPECT_EQ(decoded_d11n.cbw, expected_cbw);
  };

  // 2.4 GHz 20 MHz channels (1 to 14)
  for (uint8_t ch = 1; ch <= 14; ++ch) {
    verify_d11ac_to_d11n(ch, WL_CHANSPEC_BW_20, ch, WlanBand::kTwoGhz, ChannelBandwidth::kCbw20);
  }

  // 5 GHz 20 MHz channels
  constexpr auto five_ghz_20m_channels = std::to_array<uint8_t>(
      {36,  40,  44,  48,  52,  56,  60,  64,  100, 104, 108, 112, 116, 120,
       124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 165, 169, 173, 177});
  for (uint8_t ch : five_ghz_20m_channels) {
    verify_d11ac_to_d11n(ch, WL_CHANSPEC_BW_20, ch, WlanBand::kFiveGhz, ChannelBandwidth::kCbw20);
  }

  // There is a limitation imposed on 5GHz 40MHz channel widths.
  // //third_party/bcmdhd/crossdriver/bcmwifi_channels.cc defines the allowed 40MHz 5GHz channels
  // as
  //
  // wf_5g_40m_chans[] = {38, 46, 54, 62, 102, 110, 118, 126, 134, 142, 151, 159};

  // 5 GHz 40+ MHz channels (Cbw40 / lower primary)
  constexpr auto five_ghz_40m_plus_channels =
      std::to_array<uint8_t>({36, 44, 52, 60, 100, 108, 116, 124, 132, 140, 149, 157});
  for (uint8_t ch : five_ghz_40m_plus_channels) {
    verify_d11ac_to_d11n(ch, WL_CHANSPEC_BW_40, static_cast<uint8_t>(ch + CH_10MHZ_APART),
                         WlanBand::kFiveGhz, ChannelBandwidth::kCbw40);
  }

  // 5 GHz 40- MHz channels (Cbw40Below / upper primary)
  constexpr auto five_ghz_40m_minus_channels =
      std::to_array<uint8_t>({40, 48, 56, 64, 104, 112, 120, 128, 136, 144, 153, 161});
  for (uint8_t ch : five_ghz_40m_minus_channels) {
    verify_d11ac_to_d11n(ch, WL_CHANSPEC_BW_40, static_cast<uint8_t>(ch - CH_10MHZ_APART),
                         WlanBand::kFiveGhz, ChannelBandwidth::kCbw40Below);
  }
}

TEST_F(ChannelConversion, ChanspecD11acToD11nUnsupportedBandwidth) {
  chanspec_t d11n_chanspec = 0;

  // 80 MHz channel
  chanspec_t d11ac_80m = 0;
  ASSERT_EQ(channel2chspec(36, WL_CHANSPEC_BW_80, &d11ac_80m), ZX_OK);
  EXPECT_EQ(chanspec_d11ac_to_d11n(d11ac_80m, &d11n_chanspec), ZX_ERR_NOT_SUPPORTED);

  // 160 MHz channel
  chanspec_t d11ac_160m = 0;
  ASSERT_EQ(channel2chspec(36, WL_CHANSPEC_BW_160, &d11ac_160m), ZX_OK);
  EXPECT_EQ(chanspec_d11ac_to_d11n(d11ac_160m, &d11n_chanspec), ZX_ERR_NOT_SUPPORTED);

  // 80+80 MHz channel
  const chanspec_t d11ac_8080m = WL_CHANSPEC_BAND_5G | WL_CHANSPEC_BW_8080 |
                                 (0 << WL_CHANSPEC_CHAN1_SHIFT) | (1 << WL_CHANSPEC_CHAN2_SHIFT) |
                                 WL_CHANSPEC_CTL_SB_LL;
  EXPECT_EQ(chanspec_d11ac_to_d11n(d11ac_8080m, &d11n_chanspec), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(ChannelConversion, ChanspecD11acToD11nInvalidArgs) {
  chanspec_t d11n_chanspec = 0;

  // Nullptr output
  EXPECT_EQ(chanspec_d11ac_to_d11n(0x1006, nullptr), ZX_ERR_INVALID_ARGS);

  // Invalid band (3G)
  const chanspec_t invalid_band_chanspec = WL_CHANSPEC_BAND_3G | WL_CHANSPEC_BW_20 | 6;
  EXPECT_EQ(chanspec_d11ac_to_d11n(invalid_band_chanspec, &d11n_chanspec), ZX_ERR_INVALID_ARGS);

  // Invalid channel (> MAXCHANNEL)
  const chanspec_t invalid_chan_chanspec =
      WL_CHANSPEC_BAND_5G | WL_CHANSPEC_BW_20 | (MAXCHANNEL + 1);
  EXPECT_EQ(chanspec_d11ac_to_d11n(invalid_chan_chanspec, &d11n_chanspec), ZX_ERR_INVALID_ARGS);

  // 40 MHz with invalid sideband (> U)
  const chanspec_t invalid_sb_40m =
      WL_CHANSPEC_BAND_5G | WL_CHANSPEC_BW_40 | WL_CHANSPEC_CTL_SB_LUU | 38;
  EXPECT_EQ(chanspec_d11ac_to_d11n(invalid_sb_40m, &d11n_chanspec), ZX_ERR_INVALID_ARGS);

  // 20 MHz with invalid non-zero sideband
  const chanspec_t invalid_sb_20m =
      WL_CHANSPEC_BAND_5G | WL_CHANSPEC_BW_20 | WL_CHANSPEC_CTL_SB_U | 36;
  EXPECT_EQ(chanspec_d11ac_to_d11n(invalid_sb_20m, &d11n_chanspec), ZX_ERR_INVALID_ARGS);
}

TEST_F(ChannelConversion, D11nDecodeSuccess) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  brcmu_d11inf d11n_inf = {.io_type = BRCMU_D11N_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11n_inf), ZX_OK);

  // Test decoding legacy 20 MHz 2.4 GHz channels
  for (uint8_t ch = 1; ch <= 14; ++ch) {
    const chanspec_t legacy = CH20MHZ_LCHSPEC(ch);
    const auto result = chanspec_to_channel(&d11n_inf, legacy);
    ASSERT_TRUE(result.is_ok());
    EXPECT_EQ(result->primary.band, WlanBand::kTwoGhz);
    EXPECT_EQ(result->primary.number, ch);
    EXPECT_EQ(result->cbw, ChannelBandwidth::kCbw20);
  }

  // Test decoding legacy 20 MHz 5 GHz channels
  constexpr auto five_ghz_20m_channels = std::to_array<uint8_t>(
      {36,  40,  44,  48,  52,  56,  60,  64,  100, 104, 108, 112, 116, 120,
       124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 165, 169, 173, 177});
  for (uint8_t ch : five_ghz_20m_channels) {
    const chanspec_t legacy = CH20MHZ_LCHSPEC(ch);
    const auto result = chanspec_to_channel(&d11n_inf, legacy);
    ASSERT_TRUE(result.is_ok());
    EXPECT_EQ(result->primary.band, WlanBand::kFiveGhz);
    EXPECT_EQ(result->primary.number, ch);
    EXPECT_EQ(result->cbw, ChannelBandwidth::kCbw20);
  }

  // Test decoding legacy 40 MHz 5 GHz channels (lower and upper)
  constexpr auto center_40m_channels =
      std::to_array<uint8_t>({38, 46, 54, 62, 102, 110, 118, 126, 134, 142, 151, 159});
  for (uint8_t center : center_40m_channels) {
    // Lower sideband (CTL_SB_LOWER in legacy => primary center - 2, Cbw40)
    const chanspec_t legacy_lower =
        LCHSPEC_CREATE(center, WL_LCHANSPEC_BAND_5G, WL_LCHANSPEC_BW_40, WL_LCHANSPEC_CTL_SB_LOWER);
    const auto lower_result = chanspec_to_channel(&d11n_inf, legacy_lower);
    ASSERT_TRUE(lower_result.is_ok());
    EXPECT_EQ(lower_result->primary.band, WlanBand::kFiveGhz);
    EXPECT_EQ(lower_result->primary.number, center - CH_10MHZ_APART);
    EXPECT_EQ(lower_result->cbw, ChannelBandwidth::kCbw40);

    // Upper sideband (CTL_SB_UPPER in legacy => primary center + 2, Cbw40Below)
    const chanspec_t legacy_upper =
        LCHSPEC_CREATE(center, WL_LCHANSPEC_BAND_5G, WL_LCHANSPEC_BW_40, WL_LCHANSPEC_CTL_SB_UPPER);
    const auto upper_result = chanspec_to_channel(&d11n_inf, legacy_upper);
    ASSERT_TRUE(upper_result.is_ok());
    EXPECT_EQ(upper_result->primary.band, WlanBand::kFiveGhz);
    EXPECT_EQ(upper_result->primary.number, center + CH_10MHZ_APART);
    EXPECT_EQ(upper_result->cbw, ChannelBandwidth::kCbw40Below);
  }
}

TEST_F(ChannelConversion, D11nDecodeInvalid) {
  brcmu_d11inf d11n_inf = {.io_type = BRCMU_D11N_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11n_inf), ZX_OK);

  // Invalid channel (> MAXCHANNEL)
  const chanspec_t invalid_chan = WL_LCHANSPEC_BAND_5G | WL_LCHANSPEC_BW_20 | (MAXCHANNEL + 1);
  EXPECT_TRUE(chanspec_to_channel(&d11n_inf, invalid_chan).is_error());

  // INVCHANSPEC
  EXPECT_TRUE(chanspec_to_channel(&d11n_inf, INVCHANSPEC).is_error());
}

TEST_F(ChannelConversion, D11nEncodeUnsupportedBandwidths) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  brcmu_d11inf d11n_inf = {.io_type = BRCMU_D11N_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11n_inf), ZX_OK);

  // D11N hardware does not support VHT 80 MHz or 160 MHz.
  EXPECT_TRUE(
      channel_to_chanspec(&d11n_inf, 36, WlanBand::kFiveGhz, ChannelBandwidth::kCbw80).is_error());
  EXPECT_TRUE(
      channel_to_chanspec(&d11n_inf, 36, WlanBand::kFiveGhz, ChannelBandwidth::kCbw160).is_error());
}

TEST_F(ChannelConversion, BrcmuChanEncodeDecodeDirect) {
  using fuchsia_wlan_ieee80211::wire::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::wire::WlanBand;

  brcmu_d11inf d11ac_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11ac_inf), ZX_OK);
  ASSERT_NE(d11ac_inf.encchspec, nullptr);
  ASSERT_NE(d11ac_inf.decchspec, nullptr);

  brcmu_d11inf d11n_inf = {.io_type = BRCMU_D11N_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11n_inf), ZX_OK);
  ASSERT_NE(d11n_inf.encchspec, nullptr);
  ASSERT_NE(d11n_inf.decchspec, nullptr);

  // Encode 2.4 GHz ch 6 20MHz with D11AC and decode with D11AC
  brcmu_chan chan_ac = {
      .primary = {.band = WlanBand::kTwoGhz, .number = 6},
      .cbw = ChannelBandwidth::kCbw20,
  };
  EXPECT_EQ(d11ac_inf.encchspec(&chan_ac), ZX_OK);
  EXPECT_EQ(chan_ac.chspec, CH20MHZ_CHSPEC(6));

  brcmu_chan chan_ac_decoded = {.chspec = chan_ac.chspec};
  EXPECT_EQ(d11ac_inf.decchspec(&chan_ac_decoded), ZX_OK);
  EXPECT_EQ(chan_ac_decoded.primary.band, WlanBand::kTwoGhz);
  EXPECT_EQ(chan_ac_decoded.primary.number, 6);
  EXPECT_EQ(chan_ac_decoded.cbw, ChannelBandwidth::kCbw20);

  // Encode 2.4 GHz ch 6 20MHz with D11N and decode with D11N
  brcmu_chan chan_n = {
      .primary = {.band = WlanBand::kTwoGhz, .number = 6},
      .cbw = ChannelBandwidth::kCbw20,
  };
  EXPECT_EQ(d11n_inf.encchspec(&chan_n), ZX_OK);
  EXPECT_EQ(chan_n.chspec, CH20MHZ_LCHSPEC(6));

  brcmu_chan chan_n_decoded = {.chspec = chan_n.chspec};
  EXPECT_EQ(d11n_inf.decchspec(&chan_n_decoded), ZX_OK);
  EXPECT_EQ(chan_n_decoded.primary.band, WlanBand::kTwoGhz);
  EXPECT_EQ(chan_n_decoded.primary.number, 6);
  EXPECT_EQ(chan_n_decoded.cbw, ChannelBandwidth::kCbw20);

  // Encode 5 GHz ch 36 (40 MHz lower) with D11N and decode with D11N
  brcmu_chan chan_n_40 = {
      .primary = {.band = WlanBand::kFiveGhz, .number = 36},
      .cbw = ChannelBandwidth::kCbw40,
  };
  EXPECT_EQ(d11n_inf.encchspec(&chan_n_40), ZX_OK);
  EXPECT_NE(chan_n_40.chspec, INVCHANSPEC);

  brcmu_chan chan_n_40_decoded = {.chspec = chan_n_40.chspec};
  EXPECT_EQ(d11n_inf.decchspec(&chan_n_40_decoded), ZX_OK);
  EXPECT_EQ(chan_n_40_decoded.primary.band, WlanBand::kFiveGhz);
  EXPECT_EQ(chan_n_40_decoded.primary.number, 36);
  EXPECT_EQ(chan_n_40_decoded.cbw, ChannelBandwidth::kCbw40);
}

TEST_F(ChannelConversion, D11acDecodeInvalid) {
  brcmu_d11inf d11ac_inf = {.io_type = BRCMU_D11AC_IOTYPE};
  ASSERT_EQ(brcmu_d11_attach(&d11ac_inf), ZX_OK);

  // Null d11inf
  EXPECT_TRUE(chanspec_to_channel(nullptr, CH20MHZ_CHSPEC(6)).is_error());

  // INVCHANSPEC
  EXPECT_TRUE(chanspec_to_channel(&d11ac_inf, INVCHANSPEC).is_error());

  // Invalid band (3G)
  const chanspec_t invalid_band = WL_CHANSPEC_BAND_3G | WL_CHANSPEC_BW_20 | 6;
  EXPECT_TRUE(chanspec_to_channel(&d11ac_inf, invalid_band).is_error());

  // Invalid channel number (> MAXCHANNEL)
  const chanspec_t invalid_chan = WL_CHANSPEC_BAND_5G | WL_CHANSPEC_BW_20 | (MAXCHANNEL + 1);
  EXPECT_TRUE(chanspec_to_channel(&d11ac_inf, invalid_chan).is_error());
}

TEST_F(ChannelConversion, D11AttachInvalid) {
  EXPECT_EQ(brcmu_d11_attach(nullptr), ZX_ERR_INVALID_ARGS);

  brcmu_d11inf unknown_inf = {.io_type = 0};
  EXPECT_EQ(brcmu_d11_attach(&unknown_inf), ZX_ERR_NOT_SUPPORTED);
  EXPECT_EQ(unknown_inf.io_type, 0u);
  EXPECT_EQ(unknown_inf.encchspec, nullptr);
  EXPECT_EQ(unknown_inf.decchspec, nullptr);
}
}  // namespace
