// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
/dts-v1/;

#include "sdk/lib/driver/devicetree/visitors/drivers/interrupt-controllers/arm-gic/arm-gicv2.h"
#include "interrupts.h"

/ {
	compatible = "fuchsia,test-dt";
	model = "test";
	#address-cells = <2>;
	#size-cells = <2>;
	interrupt-parent = <&gic>;

	gic: interrupt-controller@ffffd0000 {
		compatible = "arm,gic-400";
		#interrupt-cells = <3>;
		interrupt-controller;
		reg = <0x0 0xFFFFD000 0x0 0x1000>;
	};

	device-gicv2-sgi@1 {
		compatible = "fuchsia,sample-device";
		interrupts = <GIC_SGI IRQ9_SGI IRQ9_MODE>;
		interrupt-names = "sgi-doorbell-gicv2";
		reg = <0x0 0x1 0x0 0x1>;
	};
};
