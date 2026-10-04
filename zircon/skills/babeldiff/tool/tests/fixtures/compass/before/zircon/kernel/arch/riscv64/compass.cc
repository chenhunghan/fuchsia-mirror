// A compass for a riscv64 board. Nothing here is converted.
#include <arch/compass.h>

bool Compass::IsCalibrated() const {
  // The riscv64 compass calibrates at boot.
  return calibrated_ && boot_done_;
}
