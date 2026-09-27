// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "zircon/system/utest/device-enumeration/common.h"

namespace {

TEST_F(DeviceEnumerationTest, NelsonTest) {
  static const char* kNodeMonikers[] = {
      "nelson",
      "nelson.post-init",
      "gpio.aml-gpio.gpio",
      "gpio.aml-gpio.gpio-init",
      "gpio-h.aml-gpio.gpio",
      "nelson-buttons.buttons",
      "bt-uart.aml-uart",
      "i2c-0.aml-i2c",
      "i2c-1.aml-i2c",
      "i2c-2.aml-i2c",
      "aml_gpu.aml-gpu",
      "aml-usb-phy",
      "nelson-audio-i2s-out",
      "nelson-audio-pdm-in",
      "registers",  // registers device

      // XHCI driver will not be loaded if we are in USB peripheral mode.
      // "xhci.xhci.usb-bus",

      "i2c-2.aml-i2c.i2c.i2c-2-44.backlight",
      "canvas",
      "tee.optee",
      "nelson-emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.boot1",
      "nelson-emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.boot2",
      "nelson-emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.rpmb",
      "nelson-emmc.aml-sd-emmc.sdmmc.sdmmc-mmc.user",
      "i2c-0.aml-i2c.i2c.i2c-0-57.tcs3400_light.tcs-3400",
      "aml-nna",
      "nelson-clk",
      "nelson-clk.clocks.clock-init",
      "aml-thermal-pll.thermal",
      "nelson-cpu",
      "aml-secure-mem.aml-securemem",
      "pwm.aml-pwm-device.pwm-0",
      "pwm.aml-pwm-device.pwm-1",
      "pwm.aml-pwm-device.pwm-2",
      "pwm.aml-pwm-device.pwm-3",
      "pwm.aml-pwm-device.pwm-4",
      "pwm.aml-pwm-device.pwm-5",
      "pwm.aml-pwm-device.pwm-6",
      "pwm.aml-pwm-device.pwm-7",
      "pwm.aml-pwm-device.pwm-8",
      "pwm.aml-pwm-device.pwm-9",
      "aml-sdio.aml-sd-emmc.sdmmc",
      "aml-sdio.aml-sd-emmc.sdmmc.sdmmc-sdio",
      "aml-sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1",
      "aml-sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-2",

      "display.amlogic-display.display-coordinator",
      "i2c-2.aml-i2c.i2c.i2c-2-73.ti-ina231-mlb.ti-ina231",
      "i2c-2.aml-i2c.i2c.i2c-2-64.ti-ina231-speakers.ti-ina231",
      "i2c-0.aml-i2c.i2c.i2c-0-112.shtv3",
      "goodix-gt6853-touch.goodix_gt6853_touch.goodix-gt6853",

      // Amber LED.
      "gpio-light",

      "gpio-h.aml-gpio.gpio.gpio-82.spi-1.aml-spi-1.spi.spi-1-0.selina-composite.selina",

      "aml-ram-ctl.ram",

      // Thermistor/ADC
      "thermistor.thermistor-device.therm-thread",
      "thermistor.thermistor-device.therm-audio",
      "adc.aml-saradc.0",
      "adc.aml-saradc.NELSON_THERMISTOR_THREAD",
      "adc.aml-saradc.NELSON_THERMISTOR_AUDIO",
      "adc.aml-saradc.3",

      "i2c-2.aml-i2c.i2c.i2c-2-45.tas58xx.TAS5805m",
      "i2c-2.aml-i2c.i2c.i2c-2-45.tas58xx.TAS5805m.brownout_protection",

      "gpio-c.aml-gpio.gpio.gpio-50.spi-0.aml-spi-0.spi.spi-0-0",

#ifdef include_packaged_drivers
      // OpenThread
      "gpio-c.aml-gpio.gpio.gpio-50.spi-0.aml-spi-0.spi.spi-0-0.nrf52811-radio.ot-radio",

      // WLAN
      "aml-sdio.aml-sd-emmc.sdmmc.sdmmc-sdio.sdmmc-sdio-1.wifi.brcmfmac-wlanphy",

      // Bluetooth
      "bt-uart.aml-uart.serial.bt-transport-uart",
#endif

      "i2c-2.aml-i2c.i2c.i2c-2-45.tas58xx.TAS5805m.brownout_protection.nelson-brownout-protection",

  };
  VerifyNodes(kNodeMonikers);

  static const char* kTouchscreenNodeMonikers[] = {
      // One of these touch devices could be on P0/P1 boards.
      "nelson-buttons.buttons",
      // This is the only possible touch device for P2 and beyond.
      "goodix-gt6853-touch.goodix_gt6853_touch.goodix-gt6853",
  };
  VerifyOneOf(kTouchscreenNodeMonikers);

  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/power-sensor", 2));
  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/thermal", 1));
  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/adc", 4));
  ASSERT_NO_FATAL_FAILURE(device_enumeration::WaitForClassDeviceCount("class/temperature", 2));
}

}  // namespace
