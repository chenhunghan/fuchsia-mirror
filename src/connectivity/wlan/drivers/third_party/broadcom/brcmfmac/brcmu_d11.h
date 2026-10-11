/*
 * Copyright (c) 2010 Broadcom Corporation
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

#ifndef SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_BRCMU_D11_H_
#define SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_BRCMU_D11_H_

#include <fidl/fuchsia.wlan.common/cpp/fidl.h>
#include <fidl/fuchsia.wlan.common/cpp/wire.h>
#include <fidl/fuchsia.wlan.ieee80211/cpp/fidl.h>
#include <fidl/fuchsia.wlan.ieee80211/cpp/wire.h>
#include <lib/zx/result.h>
#include <zircon/types.h>

#include "third_party/bcmdhd/crossdriver/bcmwifi_channels.h"

/* d11 io type */
#define BRCMU_D11N_IOTYPE 1
#define BRCMU_D11AC_IOTYPE 2

/* A chanspec (channel specification) holds the channel number, band,
 * bandwidth and control sideband
 */

/* chanspec binary format */

// clang-format off

#define BRCMU_CHSPEC_INVALID 255
/* bit 0~7 channel number
 * for 80+80 channels: bit 0~3 low channel id, bit 4~7 high channel id
 */
#define BRCMU_CHSPEC_CH_MASK     0x00ff
#define BRCMU_CHSPEC_CH_SHIFT    0
#define BRCMU_CHSPEC_CHL_MASK    0x000f
#define BRCMU_CHSPEC_CHL_SHIFT   0
#define BRCMU_CHSPEC_CHH_MASK    0x00f0
#define BRCMU_CHSPEC_CHH_SHIFT   4

/* bit 8~16 for dot 11n IO types
 * bit 8~9 sideband
 * bit 10~11 bandwidth
 * bit 12~13 spectral band
 * bit 14~15 not used
 */
#define BRCMU_CHSPEC_D11N_SB_MASK   0x0300
#define BRCMU_CHSPEC_D11N_SB_SHIFT  8
#define BRCMU_CHSPEC_D11N_SB_L      0x0100 /* control lower */
#define BRCMU_CHSPEC_D11N_SB_U      0x0200 /* control upper */
#define BRCMU_CHSPEC_D11N_SB_N      0x0300 /* none */
#define BRCMU_CHSPEC_D11N_BW_MASK   0x0c00
#define BRCMU_CHSPEC_D11N_BW_SHIFT  10
#define BRCMU_CHSPEC_D11N_BW_10     0x0400
#define BRCMU_CHSPEC_D11N_BW_20     0x0800
#define BRCMU_CHSPEC_D11N_BW_40     0x0c00
#define BRCMU_CHSPEC_D11N_BND_MASK  0x3000
#define BRCMU_CHSPEC_D11N_BND_SHIFT 12
#define BRCMU_CHSPEC_D11N_BND_5G    0x1000
#define BRCMU_CHSPEC_D11N_BND_2G    0x2000

/* bit 8~16 for dot 11ac IO types
 * bit 8~10 sideband
 * bit 11~13 bandwidth
 * bit 14~15 spectral band
 */
#define BRCMU_CHSPEC_D11AC_SB_MASK   0x0700
#define BRCMU_CHSPEC_D11AC_SB_SHIFT  8
#define BRCMU_CHSPEC_D11AC_SB_LLL    0x0000
#define BRCMU_CHSPEC_D11AC_SB_LLU    0x0100
#define BRCMU_CHSPEC_D11AC_SB_LUL    0x0200
#define BRCMU_CHSPEC_D11AC_SB_LUU    0x0300
#define BRCMU_CHSPEC_D11AC_SB_ULL    0x0400
#define BRCMU_CHSPEC_D11AC_SB_ULU    0x0500
#define BRCMU_CHSPEC_D11AC_SB_UUL    0x0600
#define BRCMU_CHSPEC_D11AC_SB_UUU    0x0700
#define BRCMU_CHSPEC_D11AC_SB_LL     BRCMU_CHSPEC_D11AC_SB_LLL
#define BRCMU_CHSPEC_D11AC_SB_LU     BRCMU_CHSPEC_D11AC_SB_LLU
#define BRCMU_CHSPEC_D11AC_SB_UL     BRCMU_CHSPEC_D11AC_SB_LUL
#define BRCMU_CHSPEC_D11AC_SB_UU     BRCMU_CHSPEC_D11AC_SB_LUU
#define BRCMU_CHSPEC_D11AC_SB_L      BRCMU_CHSPEC_D11AC_SB_LLL
#define BRCMU_CHSPEC_D11AC_SB_U      BRCMU_CHSPEC_D11AC_SB_LLU
#define BRCMU_CHSPEC_D11AC_BW_MASK   0x3800
#define BRCMU_CHSPEC_D11AC_BW_SHIFT  11
#define BRCMU_CHSPEC_D11AC_BW_5      0x0000
#define BRCMU_CHSPEC_D11AC_BW_10     0x0800
#define BRCMU_CHSPEC_D11AC_BW_20     0x1000
#define BRCMU_CHSPEC_D11AC_BW_40     0x1800
#define BRCMU_CHSPEC_D11AC_BW_80     0x2000
#define BRCMU_CHSPEC_D11AC_BW_160    0x2800
#define BRCMU_CHSPEC_D11AC_BW_8080   0x3000
#define BRCMU_CHSPEC_D11AC_BND_MASK  0xc000
#define BRCMU_CHSPEC_D11AC_BND_SHIFT 14
#define BRCMU_CHSPEC_D11AC_BND_2G    0x0000
#define BRCMU_CHSPEC_D11AC_BND_3G    0x4000
#define BRCMU_CHSPEC_D11AC_BND_4G    0x8000
#define BRCMU_CHSPEC_D11AC_BND_5G    0xc000

// clang-format on

struct brcmu_chan {
  chanspec_t chspec = INVCHANSPEC;
  fuchsia_wlan_ieee80211::wire::ChannelNumber primary;
  fuchsia_wlan_ieee80211::wire::ChannelBandwidth cbw;
};

/**
 * struct brcmu_d11inf - provides functions translating channel format
 *
 * @io_type: determines version of channel format used by firmware
 * @encchspec: function pointer to encode a chanspec_t from a brcmu_chan
 * @decchspec: function pointer to decode a chanspec_t to a brcmu_chan
 */
struct brcmu_d11inf {
  uint8_t io_type;
  zx_status_t (*encchspec)(struct brcmu_chan* ch);
  zx_status_t (*decchspec)(struct brcmu_chan* ch);
};

zx_status_t brcmu_d11_attach(struct brcmu_d11inf* d11inf);

zx::result<brcmu_chan> chanspec_to_channel(const brcmu_d11inf* d11inf, chanspec_t chanspec);

fuchsia_wlan_ieee80211::ChannelBandwidth enforce_bandwidth_limitations(
    uint8_t primary, fuchsia_wlan_ieee80211::WlanBand band,
    fuchsia_wlan_ieee80211::ChannelBandwidth cbw);

zx::result<chanspec_t> channel_to_chanspec(const brcmu_d11inf* d11inf, uint8_t channel,
                                           fuchsia_wlan_ieee80211::WlanBand band,
                                           fuchsia_wlan_ieee80211::ChannelBandwidth cbw);

// Convert a chanspec_t from d11ac format to d11n format.
// Returns ZX_OK on success, ZX_ERR_NOT_SUPPORTED if the chanspec has a bandwidth or feature not
// supported by d11n (e.g. 80MHz, 160MHz, 80+80MHz), or ZX_ERR_INVALID_ARGS if the chanspec is
// malformed or d11n_chanspec is nullptr.
zx_status_t chanspec_d11ac_to_d11n(chanspec_t d11ac_chanspec, chanspec_t* d11n_chanspec);

#endif  // SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_BRCMU_D11_H_
