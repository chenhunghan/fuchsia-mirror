#include <arch/compass.h>

int Compass::Heading() const {
  // Read the raw heading and correct it.
  int raw = ReadRaw();
  if (raw < 0) {
    return -1;
  }
  return (raw + offset_) % 360;
}
