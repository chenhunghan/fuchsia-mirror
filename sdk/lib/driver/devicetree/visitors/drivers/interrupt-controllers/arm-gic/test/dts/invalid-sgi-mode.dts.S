// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
/dts-v1/;

#include "sdk/lib/driver/devicetree/visitors/drivers/interrupt-controllers/arm-gic/arm-gicv3.h"
#include "interrupts.h"

/ {
	compatible = "fuchsia,test-dt";
	model = "test";
	#address-cells = <2>;
	#size-cells = <2>;
	interrupt-parent = <&gicv3>;

	gicv3: interrupt-controller@ffffe0000 {
		compatible = "arm,gic-v3";
		#interrupt-cells = <4>;
		interrupt-controller;
		reg = <0x0 0xFFFFE000 0x0 0x1000>;
	};

	device-bad-mode@1 {
		compatible = "fuchsia,sample-device";
		interrupts = <GIC_SGI IRQ9_SGI GIC_IRQ_MODE_LEVEL_HIGH 0>;
		interrupt-names = "sgi-doorbell-bad-mode";
		reg = <0x0 0x1 0x0 0x1>;
	};
};
