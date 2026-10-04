// A synthetic lantern driver for babeldiff's tests. It is not real kernel
// code; each function exists to plant one kind of conversion mistake.

use core::arch::asm;

/// The control-register value that turns the lantern off.
const LANTERN_OFF_MASK: u64 = 0x700;

/// Reads the brightness register.
pub fn read_brightness() -> u64 {
    let value: u64;
    // SAFETY: Reading the brightness register has no side effects.
    unsafe {
        asm!("mov {}, dr6", out(reg) value, options(nomem, nostack));
    }
    value
}

pub fn lantern_disable() {
    // Clearing the control register turns the lantern off.
    let value = LANTERN_OFF_MASK;
    // SAFETY: Writing the control register only affects the lantern.
    unsafe {
        asm!("mov dr7, {}", in(reg) value, options(nomem, nostack));
    }
}

pub fn glow_level(lantern: &mut Lantern) -> u32 {
    let buf_ptr = lantern.buffer.as_mut_ptr();
    // SAFETY: The buffer holds the level at offset 0.
    let level = unsafe { &mut *(buf_ptr as *mut u32) };
    *level
}
