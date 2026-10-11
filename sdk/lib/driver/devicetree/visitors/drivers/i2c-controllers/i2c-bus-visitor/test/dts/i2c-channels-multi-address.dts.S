// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file
#include "i2c.h"

/dts-v1/;

/ {
	compatible = "fuchsia,test-dt";
	model = "test";
	#address-cells = <2>;
	#size-cells = <2>;

	i2c@ffffa000 {
		compatible = "fuchsia,i2c";
		reg = <0x0 0xffffa000 0x0 0x1000>;
		#address-cells = <1>;
		#size-cells = <0>;

		child_2b: child@2b {
			reg = <I2C_ADDRESS3 I2C_ADDRESS4>;
		};
	};

	// child@2b has two I2C addresses, so the reference is ambiguous.
	consumer@ffffb000 {
		compatible = "fuchsia,test-consumer";
		reg = <0x0 0xffffb000 0x0 0x1000>;
		i2c-channels = <&child_2b>;
		i2c-channel-names = "consumer-2b";
	};
};
