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

	// Before the I2C buses. References channels on two buses, in property order.
	consumer-before@ffff9000 {
		compatible = "fuchsia,test-consumer";
		reg = <0x0 0xffff9000 0x0 0x1000>;
		i2c-channels = <&bus1_pmic_3a>, <&bus0_pmic_38>;
		i2c-channel-names = "bus1-pmic", "bus0-pmic";
	};

	i2c@ffffa000 {
		compatible = "fuchsia,i2c";
		reg = <0x0 0xffffa000 0x0 0x1000>;
		#address-cells = <1>;
		#size-cells = <0>;

		child@c {
			compatible = "fuchsia,test";
			reg = <I2C_ADDRESS1>;
		};

		bus0_pmic_1e: pmic@1e {
			reg = <I2C_ADDRESS2>;
		};

		// An I2C child that references a sibling and a child of another bus.
		child@2b {
			compatible = "fuchsia,test";
			reg = <I2C_ADDRESS3>;
			i2c-channels = <&bus0_pmic_1e>, <&bus1_pmic_30>;
			i2c-channel-names = "bus0-pmic", "bus1-pmic";
		};

		bus0_pmic_38: pmic@38 {
			reg = <I2C_ADDRESS6>;
		};
	};

	i2c@ffffc000 {
		compatible = "fuchsia,i2c";
		reg = <0x0 0xffffc000 0x0 0x1000>;
		#address-cells = <1>;
		#size-cells = <0>;

		bus1_pmic_30: pmic@30 {
			reg = <I2C_ADDRESS5>;
		};

		bus1_pmic_3a: pmic@3a {
			reg = <I2C_ADDRESS4>;
		};

		bus1_pmic_c: pmic@c {
			reg = <I2C_ADDRESS1>;
		};
	};

	// After the I2C buses.
	consumer-after@ffffd000 {
		compatible = "fuchsia,test-consumer";
		reg = <0x0 0xffffd000 0x0 0x1000>;
		i2c-channels = <&bus1_pmic_c>;
		i2c-channel-names = "bus1-pmic";
	};
};
