// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
/dts-v1/;

/ {
	test-board {
		test-soc {
			pmic: test-pmic {
				compatible = "some-pmic";

				pmic_usb: usb-power {
					power-supply-type = "usb";
					power-supply-output-name = "chgin";
				};

				pmic_wireless: wireless-power {
					power-supply-type = "usb";
					power-supply-output-name = "wcin";
				};
			};

			test_device: test-device {
				compatible = "test-usb-phy";
				power-supplies = <&pmic_usb>;
				power-supply-names = "usb-power";
			};

			multi_supply_device: multi-supply-device {
				compatible = "test-usb-phy";
				power-supplies = <&pmic_usb>, <&pmic_wireless>;
				power-supply-names = "wired", "wireless";
			};

			unnamed_supply_device: unnamed-supply-device {
				compatible = "test-usb-phy";
				power-supplies = <&pmic_usb>;
			};
		};
	};
};
