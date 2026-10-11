/*
 * Copyright (c) 2013 Broadcom Corporation
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
/*********************channel spec common functions*********************/

#include <fidl/fuchsia.wlan.common/cpp/wire.h>
#include <fidl/fuchsia.wlan.ieee80211/cpp/fidl.h>
#include <fidl/fuchsia.wlan.ieee80211/cpp/wire.h>
#include <zircon/assert.h>

#include <third_party/bcmdhd/crossdriver/bcmwifi_channels.h>
#include <third_party/bcmdhd/crossdriver/wl_cfg80211.h>

#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_d11.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/debug.h"

static zx_status_t brcmu_d11ac_encchspec(struct brcmu_chan* ch) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;

  if (ch == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }

  // Some scenarios require specific bandwidth overrides.
  const auto cbw_override =
      enforce_bandwidth_limitations(ch->primary.number, static_cast<WlanBand>(ch->primary.band),
                                    static_cast<ChannelBandwidth>(ch->cbw));

  chanspec_t bandwidth = 0;
  switch (cbw_override) {
    case ChannelBandwidth::kCbw20:
      bandwidth = WL_CHANSPEC_BW_20;
      break;
    case ChannelBandwidth::kCbw40:
      [[fallthrough]];
    case ChannelBandwidth::kCbw40Below:
      bandwidth = WL_CHANSPEC_BW_40;
      break;
    case ChannelBandwidth::kCbw80:
      bandwidth = WL_CHANSPEC_BW_80;
      break;
    case ChannelBandwidth::kCbw160:
      bandwidth = WL_CHANSPEC_BW_160;
      break;
    case ChannelBandwidth::kCbw80P80:
      bandwidth = WL_CHANSPEC_BW_8080;
      break;
    default:
      BRCMF_ERR("Unsupported channel bandwidth: %u", static_cast<uint32_t>(cbw_override));
      ch->chspec = INVCHANSPEC;
      return ZX_ERR_NOT_SUPPORTED;
  }

  chanspec_t chanspec = 0;
  const zx_status_t status = channel2chspec(ch->primary.number, bandwidth, &chanspec);
  if (status != ZX_OK || chspec_malformed(chanspec)) {
    ch->chspec = INVCHANSPEC;
    return status != ZX_OK ? status : ZX_ERR_INVALID_ARGS;
  }
  ch->chspec = chanspec;
  return ZX_OK;
}

static zx_status_t brcmu_d11ac_decchspec(struct brcmu_chan* ch) {
  if (ch == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }

  const chanspec_t chanspec = ch->chspec;
  if (chanspec == INVCHANSPEC) {
    return ZX_ERR_INVALID_ARGS;
  }

  fuchsia_wlan_ieee80211::wire::WlanBand band;
  if (CHSPEC_IS2G(chanspec)) {
    band = fuchsia_wlan_ieee80211::wire::WlanBand::kTwoGhz;
  } else if (CHSPEC_IS5G(chanspec)) {
    band = fuchsia_wlan_ieee80211::wire::WlanBand::kFiveGhz;
  } else {
    return ZX_ERR_INVALID_ARGS;
  }

  uint8_t ctl_chan = 0;
  const zx_status_t status = chspec_ctlchan(chanspec, &ctl_chan);
  if (status != ZX_OK) {
    BRCMF_ERR("Failed to get control channel from chanspec: 0x%x status: %d", chanspec, status);
    return status;
  }
  ch->primary = {.band = band, .number = ctl_chan};

  if (CHSPEC_IS20(chanspec)) {
    ch->cbw = fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw20;
  } else if (CHSPEC_IS40(chanspec)) {
    // These macros describe whether the PRIMARY (or in brcmfmac parlance "control") channel is
    // above or below the side band.  If the primary channel is the upper, then the side band is
    // lower (eg: 40-).  And if the primary channel is the lower, then the side band is upper
    // (eg: 36+).
    const uint16_t sb = chanspec & WL_CHANSPEC_CTL_SB_MASK;
    if (sb == WL_CHANSPEC_CTL_SB_L) {
      ch->cbw = fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw40;
    } else if (sb == WL_CHANSPEC_CTL_SB_U) {
      ch->cbw = fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw40Below;
    } else {
      BRCMF_ERR("unsupported channel side band: %u", sb);
      return ZX_ERR_NOT_SUPPORTED;
    }
  } else if (CHSPEC_IS80(chanspec)) {
    ch->cbw = fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw80;
  } else if (CHSPEC_IS160(chanspec)) {
    ch->cbw = fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw160;
  } else if (CHSPEC_IS8080(chanspec)) {
    ch->cbw = fuchsia_wlan_ieee80211::wire::ChannelBandwidth::kCbw80P80;
  } else {
    BRCMF_ERR("unsupported channel width in chanspec: 0x%x", chanspec);
    return ZX_ERR_NOT_SUPPORTED;
  }
  return ZX_OK;
}

static zx_status_t brcmu_d11n_decchspec(struct brcmu_chan* ch) {
  if (ch == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  const chanspec_t converted = chspec_from_legacy(ch->chspec);
  if (converted == INVCHANSPEC) {
    return ZX_ERR_INVALID_ARGS;
  }
  ch->chspec = converted;
  return brcmu_d11ac_decchspec(ch);
}

static zx_status_t brcmu_d11n_encchspec(struct brcmu_chan* ch) {
  if (ch == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  const zx_status_t status = brcmu_d11ac_encchspec(ch);
  if (status != ZX_OK) {
    return status;
  }
  chanspec_t d11n_chanspec = 0;
  const zx_status_t conv_status = chanspec_d11ac_to_d11n(ch->chspec, &d11n_chanspec);
  if (conv_status != ZX_OK) {
    ch->chspec = INVCHANSPEC;
    return conv_status;
  }
  ch->chspec = d11n_chanspec;
  return ZX_OK;
}

zx::result<chanspec_t> channel_to_chanspec(const brcmu_d11inf* d11inf, uint8_t channel,
                                           fuchsia_wlan_ieee80211::WlanBand band,
                                           fuchsia_wlan_ieee80211::ChannelBandwidth cbw) {
  if (d11inf == nullptr || d11inf->encchspec == nullptr) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  brcmu_chan ch = {
      .chspec = INVCHANSPEC,
      .primary = {.band = static_cast<fuchsia_wlan_ieee80211::wire::WlanBand>(band),
                  .number = channel},
      .cbw = static_cast<fuchsia_wlan_ieee80211::wire::ChannelBandwidth>(cbw),
  };
  const zx_status_t status = d11inf->encchspec(&ch);
  if (status != ZX_OK || ch.chspec == INVCHANSPEC) {
    return zx::error(status != ZX_OK ? status : ZX_ERR_INVALID_ARGS);
  }

  return zx::ok(ch.chspec);
}

zx::result<brcmu_chan> chanspec_to_channel(const brcmu_d11inf* d11inf, chanspec_t chanspec) {
  if (d11inf == nullptr || d11inf->decchspec == nullptr) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  brcmu_chan ch = {.chspec = chanspec};
  const zx_status_t status = d11inf->decchspec(&ch);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(ch);
}

zx_status_t brcmu_d11_attach(struct brcmu_d11inf* d11inf) {
  if (d11inf == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  switch (d11inf->io_type) {
    case BRCMU_D11N_IOTYPE:
      d11inf->encchspec = brcmu_d11n_encchspec;
      d11inf->decchspec = brcmu_d11n_decchspec;
      return ZX_OK;
    case BRCMU_D11AC_IOTYPE:
      d11inf->encchspec = brcmu_d11ac_encchspec;
      d11inf->decchspec = brcmu_d11ac_decchspec;
      return ZX_OK;
    default:
      BRCMF_ERR("Unsupported D11 io_type: %u", d11inf->io_type);
      return ZX_ERR_NOT_SUPPORTED;
  }
}

fuchsia_wlan_ieee80211::ChannelBandwidth enforce_bandwidth_limitations(
    uint8_t primary, fuchsia_wlan_ieee80211::WlanBand band,
    fuchsia_wlan_ieee80211::ChannelBandwidth cbw) {
  using fuchsia_wlan_ieee80211::ChannelBandwidth;
  using fuchsia_wlan_ieee80211::WlanBand;

  // The chip and firmware appear to only support 20MHz connections on 2.4GHz.
  if (band == WlanBand::kTwoGhz) {
    return ChannelBandwidth::kCbw20;
  }

  // Override the channel bandwidth with 20Mhz because `channel2chanspec` doesn't support
  // encoding 80+80 Mhz, and we have always overridden to 20Mhz in this case.
  // TODO(https://fxbug.dev/42144507) - Remove this override.
  if (cbw == ChannelBandwidth::kCbw80P80) {
    return ChannelBandwidth::kCbw20;
  }

  // Connecting to channels >= 165 with bandwidths > 20MHz is not supported per fxrev.dev/1446009.
  if (band == WlanBand::kFiveGhz && primary >= 165 && cbw != ChannelBandwidth::kCbw20) {
    return ChannelBandwidth::kCbw20;
  }

  return cbw;
}

zx_status_t chanspec_d11ac_to_d11n(chanspec_t d11ac_chanspec, chanspec_t* d11n_chanspec) {
  if (d11n_chanspec == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (chspec_malformed(d11ac_chanspec)) {
    return ZX_ERR_INVALID_ARGS;
  }

  uint16_t d11n_bw = 0;
  uint16_t d11n_sb = 0;

  if (CHSPEC_IS20(d11ac_chanspec)) {
    d11n_bw = BRCMU_CHSPEC_D11N_BW_20;
    d11n_sb = BRCMU_CHSPEC_D11N_SB_N;
  } else if (CHSPEC_IS40(d11ac_chanspec)) {
    d11n_bw = BRCMU_CHSPEC_D11N_BW_40;
    const uint16_t sb = d11ac_chanspec & WL_CHANSPEC_CTL_SB_MASK;
    if (sb == WL_CHANSPEC_CTL_SB_L) {
      d11n_sb = BRCMU_CHSPEC_D11N_SB_L;
    } else if (sb == WL_CHANSPEC_CTL_SB_U) {
      d11n_sb = BRCMU_CHSPEC_D11N_SB_U;
    } else {
      return ZX_ERR_INVALID_ARGS;
    }
  } else {
    // 80MHz, 160MHz, 80+80MHz and other bandwidths are not supported by d11n.
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint16_t d11n_band = 0;
  if (CHSPEC_IS2G(d11ac_chanspec)) {
    d11n_band = BRCMU_CHSPEC_D11N_BND_2G;
  } else if (CHSPEC_IS5G(d11ac_chanspec)) {
    d11n_band = BRCMU_CHSPEC_D11N_BND_5G;
  } else {
    return ZX_ERR_INVALID_ARGS;
  }

  const uint8_t chan = d11ac_chanspec & WL_CHANSPEC_CHAN_MASK;
  *d11n_chanspec = static_cast<chanspec_t>(d11n_band | d11n_bw | d11n_sb | chan);
  return ZX_OK;
}
