// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::traits::HasPolicyId;
use super::{
    NewPolicy, OBJECT_R_ROLE_NAME, PolicyCap, RoleId, SecurityContext, SecurityContextError,
};
use crate::{InitialSid, NullessByteStr};

impl NewPolicy {
    /// Returns true if the specified capability is in the policy's enabled capabilities set.
    pub fn has_policycap(&self, policy_cap: PolicyCap) -> bool {
        self.policy_capabilities().contains(policy_cap)
    }

    /// Returns the [`RoleId`] of the `"object_r"` role within the policy, for use when validating
    /// and computing Security Context fields.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::validate`] has not verified the presence of `"object_r"`.
    pub(crate) fn object_role(&self) -> RoleId {
        self.roles()
            .get_by_name(OBJECT_R_ROLE_NAME)
            .expect("validated policy must contain object_r role")
            .id()
    }

    /// Returns the [`SecurityContext`] defined by this policy for the specified [`InitialSid`].
    ///
    /// # Panics
    ///
    /// Panics if [`Self::validate`] has not verified that all required initial SIDs are present.
    pub fn initial_context(&self, mut id: InitialSid) -> SecurityContext {
        let need_init_sid = self.has_policycap(PolicyCap::UserspaceInitialContext);
        if id == InitialSid::Init && !need_init_sid {
            id = InitialSid::Kernel;
        }
        let context = self
            .initial_sids()
            .get_by_id(id as u32)
            .expect("initial SID must be present in validated policy");
        SecurityContext::from_policy_context(context)
    }

    /// Returns a [`SecurityContext`] with fields parsed from the supplied Security Context string.
    pub fn parse_security_context(
        &self,
        security_context: NullessByteStr<'_>,
    ) -> Result<SecurityContext, SecurityContextError> {
        SecurityContext::from_string(self, security_context)
    }

    /// Validates a [`SecurityContext`] against this policy's constraints.
    pub fn validate_security_context(
        &self,
        security_context: &SecurityContext,
    ) -> Result<(), SecurityContextError> {
        security_context.validate(self)
    }

    /// Returns a byte string describing the supplied [`SecurityContext`].
    pub fn serialize_security_context(&self, security_context: &SecurityContext) -> Vec<u8> {
        security_context.to_string(self)
    }
}
