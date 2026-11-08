// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

fn main() {
    let constraints = fidl_fuchsia_sysmem2::BufferCollectionConstraints::default();
    assert_eq!(constraints, fidl_fuchsia_sysmem2::BufferCollectionConstraints::default());

    // Verify the FIDL and Rust API levels are consistent and working correctly.
    #[cfg(fuchsia_api_level_at_least = "PLATFORM")]
    let y_value: fidl_fuchsia_examples::AvailableFromApiLevelNext = {
        // Both aliases are available in the platform build.
        let before_next: fidl_fuchsia_examples::AvailableUntilApiLevelNext = 10;
        10000 + before_next
    };
    #[cfg(all(
        fuchsia_api_level_at_least = "NEXT",  // NEVER_REPLACE_NEXT
        fuchsia_api_level_less_than = "PLATFORM"
    ))]
    let y_value: fidl_fuchsia_examples::AvailableFromApiLevelNext = 10000;
    #[cfg(fuchsia_api_level_less_than = "NEXT")] // NEVER_REPLACE_NEXT
    let y_value: fidl_fuchsia_examples::AvailableUntilApiLevelNext = 30;

    let vec = fidl_fuchsia_math::Vec_ { x: 1, y: y_value };
    assert_eq!(vec.x + vec.y, 1 + y_value);

    use fidl_next_convert_fuchsia_math as _;
    let vec_next = fidl_next_fuchsia_math::Vec { x: 1, y: y_value };
    assert_eq!(vec_next.x + vec_next.y, 1 + y_value);

    #[cfg(target_os = "fuchsia")]
    {
        let loc_next = fidl_next_convert_fuchsia_examples::location_type_into_rust_next(
            fidl_fuchsia_examples::LocationType::Museum,
        );
        assert_eq!(loc_next, fidl_next_fuchsia_examples::LocationType::Museum);
    }

    #[cfg(fuchsia_api_level_at_least = "PLATFORM")]
    {
        let vec_fdomain_next = fdomain_next_fuchsia_math::Vec { x: 1, y: y_value };
        assert_eq!(vec_fdomain_next.x + vec_fdomain_next.y, 1 + y_value);

        let loc_fdomain_next = fdomain_next_fuchsia_examples::LocationType::Museum;
        assert_eq!(loc_fdomain_next, fdomain_next_fuchsia_examples::LocationType::Museum);
    }
}
