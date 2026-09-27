// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/wlan/lib/common/cpp/include/wlan/common/ieee80211_codes.h"

#include <zircon/assert.h>

namespace wlan {
namespace common {

namespace wlan_ieee80211 = ::fuchsia_wlan_ieee80211;

namespace {

template <typename T>
constexpr bool IsValidStatusCode(T status_code) {
  switch (status_code) {
    case static_cast<T>(wlan_ieee80211::StatusCode::kSuccess):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedReasonUnspecified):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTdlsRejectedAlternativeProvided):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTdlsRejected):
    case static_cast<T>(wlan_ieee80211::StatusCode::kSecurityDisabled):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUnacceptableLifetime):
    case static_cast<T>(wlan_ieee80211::StatusCode::kNotInSameBss):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedCapabilitiesMismatch):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedNoAssociationExists):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedOtherReason):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUnsupportedAuthAlgorithm):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTransactionSequenceError):
    case static_cast<T>(wlan_ieee80211::StatusCode::kChallengeFailure):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedSequenceTimeout):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedNoMoreStas):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedBasicRatesMismatch):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedNoShortPreambleSupport):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedSpectrumManagementRequired):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedBadPowerCapability):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedBadSupportedChannels):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedNoShortSlotTimeSupport):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedNoHtSupport):
    case static_cast<T>(wlan_ieee80211::StatusCode::kR0KhUnreachable):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedPcoTimeNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedTemporarily):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRobustManagementPolicyViolation):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUnspecifiedQosFailure):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedInsufficientBandwidth):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedPoorChannelConditions):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedQosNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRequestDeclined):
    case static_cast<T>(wlan_ieee80211::StatusCode::kInvalidParameters):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedWithSuggestedChanges):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidElement):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidGroupCipher):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidPairwiseCipher):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidAkmp):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUnsupportedRsneVersion):
    case static_cast<T>(wlan_ieee80211::StatusCode::kInvalidRsneCapabilities):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusCipherOutOfPolicy):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedForDelayPeriod):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDlsNotAllowed):
    case static_cast<T>(wlan_ieee80211::StatusCode::kNotPresent):
    case static_cast<T>(wlan_ieee80211::StatusCode::kNotQosSta):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedListenIntervalTooLarge):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidFtActionFrameCount):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidPmkid):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidMde):
    case static_cast<T>(wlan_ieee80211::StatusCode::kStatusInvalidFte):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRequestedTclasNotSupportedByAp):
    case static_cast<T>(wlan_ieee80211::StatusCode::kInsufficientTclasProcessingResources):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTryAnotherBss):
    case static_cast<T>(wlan_ieee80211::StatusCode::kGasAdvertisementProtocolNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kNoOutstandingGasRequest):
    case static_cast<T>(wlan_ieee80211::StatusCode::kGasResponseNotReceivedFromServer):
    case static_cast<T>(wlan_ieee80211::StatusCode::kGasQueryTimeout):
    case static_cast<T>(wlan_ieee80211::StatusCode::kGasQueryResponseTooLarge):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedHomeWithSuggestedChanges):
    case static_cast<T>(wlan_ieee80211::StatusCode::kServerUnreachable):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedForSspPermissions):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedUnauthenticatedAccessNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kInvalidRsne):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUApsdCoexistanceNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUApsdCoexModeNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kBadIntervalWithUApsdCoex):
    case static_cast<T>(wlan_ieee80211::StatusCode::kAntiCloggingTokenRequired):
    case static_cast<T>(wlan_ieee80211::StatusCode::kUnsupportedFiniteCyclicGroup):
    case static_cast<T>(wlan_ieee80211::StatusCode::kCannotFindAlternativeTbtt):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTransmissionFailure):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRequestedTclasNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTclasResourcesExhausted):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedWithSuggestedBssTransition):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectWithSchedule):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectNoWakeupSpecified):
    case static_cast<T>(wlan_ieee80211::StatusCode::kSuccessPowerSaveMode):
    case static_cast<T>(wlan_ieee80211::StatusCode::kPendingAdmittingFstSession):
    case static_cast<T>(wlan_ieee80211::StatusCode::kPerformingFstNow):
    case static_cast<T>(wlan_ieee80211::StatusCode::kPendingGapInBaWindow):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectUPidSetting):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedExternalReason):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRefusedApOutOfMemory):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectedEmergencyServicesNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kQueryResponseOutstanding):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRejectDseBand):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTclasProcessingTerminated):
    case static_cast<T>(wlan_ieee80211::StatusCode::kTsScheduleConflict):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedWithSuggestedBandAndChannel):
    case static_cast<T>(wlan_ieee80211::StatusCode::kMccaopReservationConflict):
    case static_cast<T>(wlan_ieee80211::StatusCode::kMafLimitExceeded):
    case static_cast<T>(wlan_ieee80211::StatusCode::kMccaTrackLimitExceeded):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedDueToSpectrumManagement):
    case static_cast<T>(wlan_ieee80211::StatusCode::kDeniedVhtNotSupported):
    case static_cast<T>(wlan_ieee80211::StatusCode::kEnablementDenied):
    case static_cast<T>(wlan_ieee80211::StatusCode::kRestrictionFromAuthorizedGdb):
    case static_cast<T>(wlan_ieee80211::StatusCode::kAuthorizationDeenabled):
      return true;
    default:
      return false;
  }
}

}  // namespace

uint16_t ConvertStatusCode(wlan_ieee80211::StatusCode status) {
  ZX_ASSERT(IsValidStatusCode(static_cast<uint16_t>(status)));
  return static_cast<uint16_t>(status);
}

wlan_ieee80211::StatusCode ConvertStatusCode(uint16_t status) {
  // Use a default for invalid uint16_t status codes from external sources.
  if (!IsValidStatusCode(status)) {
    return wlan_ieee80211::StatusCode::kRefusedReasonUnspecified;
  }
  return static_cast<wlan_ieee80211::StatusCode>(status);
}

}  // namespace common
}  // namespace wlan
