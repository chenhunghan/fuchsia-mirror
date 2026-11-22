//! Internal native stack probing for pest.
//!
//! Stack probing is automatic on the six native targets listed in this crate's
//! manifest. Other targets require the `stack-guard` feature and a toolchain
//! supported by `stacker` and `psm`.

#![warn(missing_docs, rust_2018_idioms, unused_qualifications)]

/// Returns the remaining native stack space in bytes, if known.
///
/// Returns `None` when stack probing is disabled or stack bounds are unavailable.
#[inline]
pub fn remaining_stack() -> Option<usize> {
    #[cfg(any(native_stack_guard, feature = "stack-guard"))]
    return stacker::remaining_stack();

    #[cfg(not(any(native_stack_guard, feature = "stack-guard")))]
    return None;
}

#[cfg(test)]
mod tests {
    use super::remaining_stack;

    #[cfg(native_stack_guard)]
    #[test]
    fn native_stack_bounds_are_available() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                assert!(remaining_stack().is_some_and(|remaining| remaining > 64 * 1024));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[cfg(all(not(native_stack_guard), feature = "stack-guard"))]
    #[test]
    fn explicit_stack_probe_uses_stacker() {
        assert_eq!(
            remaining_stack().is_some(),
            stacker::remaining_stack().is_some()
        );
    }

    #[cfg(not(any(native_stack_guard, feature = "stack-guard")))]
    #[test]
    fn disabled_stack_probe_returns_none() {
        assert_eq!(remaining_stack(), None);
    }
}
