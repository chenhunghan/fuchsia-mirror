impl Compass {
    pub fn heading(&self) -> i32 {
        // Read the raw heading and correct it.
        let raw = self.read_raw();
        if raw < 0 {
            return -1;
        }
        (raw + self.offset) % 360
    }

    pub fn is_calibrated(&self) -> bool {
        // The x86 compass is calibrated once the offset is known.
        self.calibrated && self.offset_known
    }
}
