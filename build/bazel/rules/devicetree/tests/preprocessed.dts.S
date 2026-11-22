// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <test_dep/dep_header.h>
#include "local_header.h"

/dts-v1/;

/ {
	compatible = "fuchsia,test-dt";
	model = "preprocessed";
	#address-cells = <1>;
	#size-cells = <1>;

	first@2000 {
		reg = <LOCAL_REG_ADDR DEP_REG_SIZE>;
#ifdef TEST_DEP_DEFINE
		status = "okay";
#endif
	};

	second@2000 {
		reg = <LOCAL_REG_ADDR DEP_REG_SIZE>;
	};
};

/include/ "local_fragment.dtsi"
