// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(not(lib_target_cfg))]
compile_error!("expected --cfg=lib_target_cfg");

pub fn env_value() -> &'static str {
    env!("LIB_TARGET_ENV")
}

#[cfg(test)]
mod tests {
    use super::env_value;

    #[test]
    fn check_flags_and_env() {
        #[cfg(expect_replaced_env)]
        assert_eq!(env_value(), "from_replacement");
        #[cfg(not(expect_replaced_env))]
        assert_eq!(env_value(), "from_crate");
    }
}
