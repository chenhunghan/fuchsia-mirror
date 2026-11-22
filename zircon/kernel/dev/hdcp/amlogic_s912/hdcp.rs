// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::arm64::periph_paddr_to_vaddr;
use crate::arch_rs::arm64::smccc::arm_smccc_smc;
use crate::kernel::types::PAddr;
use crate::top::debug::spin_usecs;
use core::ptr::with_exposed_provenance_mut;
use regio::{Mmio, MmioBank, MmioPtr, Offset, RwSafe};
use zbi::DcfgAmlogicHdcpDriver;

// Layout assertions matching `zbi_dcfg_amlogic_hdcp_driver_t` in `abi_type_validator.cc`.
zr::static_assert_size_and_align!(DcfgAmlogicHdcpDriver, 24, 8);
zr::static_assert!(core::mem::offset_of!(DcfgAmlogicHdcpDriver, preset_phys) == 0);
zr::static_assert!(core::mem::offset_of!(DcfgAmlogicHdcpDriver, hiu_phys) == 8);
zr::static_assert!(core::mem::offset_of!(DcfgAmlogicHdcpDriver, hdmitx_phys) == 16);

const TOP_OFFSET_MASK: u32 = 0x0 << 24;
const DWC_OFFSET_MASK: u32 = 0x10 << 24;
const HDMITX_ADDR_PORT: usize = 0x00;
const HDMITX_DATA_PORT: usize = 0x04;

const HHI_HDMI_CLK_CNTL: Offset<u32, RwSafe> = Offset::new(0x73 << 2);
const HHI_GCLK_MPEG2: Offset<u32, RwSafe> = Offset::new(0x52 << 2);
const HHI_MEM_PD_REG0: Offset<u32, RwSafe> = Offset::new(0x40 << 2);
const PRESET0_REGISTER: Offset<u32, RwSafe> = Offset::new(0x404);
const PRESET2_REGISTER: Offset<u32, RwSafe> = Offset::new(0x40C);
#[allow(clippy::identity_op, reason = "keeps the register offset explicit")]
const HDMITX_TOP_SW_RESET: u32 = TOP_OFFSET_MASK + 0x000;
const HDMITX_TOP_CLK_CNTL: u32 = TOP_OFFSET_MASK + 0x001;
const HDMITX_DWC_MC_LOCKONCLOCK: u32 = DWC_OFFSET_MASK + 0x4006;
const HDMITX_DWC_MC_CLKDIS: u32 = DWC_OFFSET_MASK + 0x4001;
const HDMITX_DWC_A_APIINTMSK: u32 = DWC_OFFSET_MASK + 0x5008;
const HDMITX_DWC_A_VIDPOLCFG: u32 = DWC_OFFSET_MASK + 0x5009;
const HDMITX_DWC_A_OESSWCFG: u32 = DWC_OFFSET_MASK + 0x500A;

/// Size of the HIU register bank, spanning through `HHI_HDMI_CLK_CNTL` (0x1cc).
const HIU_BANK_SIZE: usize = 0x1d0;
/// Size of the PRESET register bank, spanning through `PRESET2_REGISTER` (0x40c).
const PRESET_BANK_SIZE: usize = 0x410;
/// Size of the HDMITX register bank, spanning through the DWC data port (0x14).
const HDMITX_BANK_SIZE: usize = 0x18;

/// A 32-bit read/write MMIO register projected from one of the S912 register banks.
type Reg32 = Mmio<u32, u32, RwSafe>;

/// Returns a mask of `count` bits starting at bit `start`.
const fn display_mask(start: u32, count: u32) -> u32 {
    ((1 << count) - 1) << start
}

/// Returns `mask` with its `count` bits starting at bit `start` replaced by the low bits of
/// `value`.
const fn display_set_mask(mask: u32, start: u32, count: u32, value: u32) -> u32 {
    (mask & !display_mask(start, count)) | ((value << start) & display_mask(start, count))
}

/// Replaces the `count` bits starting at bit `start` of `reg` with `value`.
fn set_bit32(reg: Reg32, value: u32, count: u32, start: u32) {
    reg.modify(|v| *v = display_set_mask(*v, start, count, value));
}

/// Writes `data` to HDMITX register `addr` through the TOP or DWC address/data port pair.
///
/// Each of `top` and `dwc` is `[addr port, data port]`.
fn hdmitx_writereg(top: [Reg32; 2], dwc: [Reg32; 2], addr: u32, data: u32) {
    // determine if we are writing to HDMI TOP (AMLOGIC Wrapper) or HDMI IP
    let [addr_port, data_port] = if addr & DWC_OFFSET_MASK != 0 { dwc } else { top };
    let addr = addr & 0xffff;
    addr_port.write(addr);
    addr_port.write(addr); // FIXME: Need to write twice!
    data_port.write(data);
}

/// Initializes the driver.
///
/// # Safety
///
/// Must only be called during platform device initialization. The physical addresses in
/// `config` must be the Amlogic S912 PRESET, HIU and HDMITX register blocks (each 4-byte
/// aligned), covered by the peripheral memory map.
unsafe fn amlogic_s912_hdcp_init(config: &DcfgAmlogicHdcpDriver) {
    assert!(config.preset_phys != 0);
    assert!(config.hiu_phys != 0);
    assert!(config.hdmitx_phys != 0);

    // get virtual addresses of our peripheral bases
    let preset_base = periph_paddr_to_vaddr(PAddr(config.preset_phys as usize)).map_or(0, |v| v.0);
    let hiu_base = periph_paddr_to_vaddr(PAddr(config.hiu_phys as usize)).map_or(0, |v| v.0);
    let hdmitx_base = periph_paddr_to_vaddr(PAddr(config.hdmitx_phys as usize)).map_or(0, |v| v.0);
    assert!(preset_base != 0);
    assert!(hiu_base != 0);
    assert!(hdmitx_base != 0);

    let preset = MmioBank::new(
        // SAFETY: `periph_paddr_to_vaddr` returned a non-null address in the peripheral map,
        // which stays mapped for the lifetime of the kernel. The caller guarantees it is the
        // 4-byte aligned S912 PRESET block, whose 32-bit registers span `PRESET_BANK_SIZE` bytes,
        // never trap, and accept any written value without undefined behavior.
        unsafe { MmioPtr::<u32, RwSafe>::new(with_exposed_provenance_mut(preset_base)) },
        PRESET_BANK_SIZE,
    );
    let hiu = MmioBank::new(
        // SAFETY: As above, for the S912 HIU block of `HIU_BANK_SIZE` bytes.
        unsafe { MmioPtr::<u32, RwSafe>::new(with_exposed_provenance_mut(hiu_base)) },
        HIU_BANK_SIZE,
    );
    let hdmitx = MmioBank::new(
        // SAFETY: As above, for the S912 HDMITX block of `HDMITX_BANK_SIZE` bytes.
        unsafe { MmioPtr::<u32, RwSafe>::new(with_exposed_provenance_mut(hdmitx_base)) },
        HDMITX_BANK_SIZE,
    );

    // SAFETY: `hiu` was constructed above from an aligned pointer attested valid for
    // `HIU_BANK_SIZE` bytes, and `at` checks that each `HHI_*` offset is aligned and within it.
    let (hhi_hdmi_clk_cntl, hhi_gclk_mpeg2, hhi_mem_pd_reg0) =
        unsafe { (hiu.at(HHI_HDMI_CLK_CNTL), hiu.at(HHI_GCLK_MPEG2), hiu.at(HHI_MEM_PD_REG0)) };

    // enable clocks
    set_bit32(hhi_hdmi_clk_cntl, 0x0100, 16, 0);

    // enable clk81 (needed for HDMI module and a bunch of other modules)
    set_bit32(hhi_gclk_mpeg2, 1, 1, 4);

    // power up HDMI Memory (bits 15:8)
    set_bit32(hhi_mem_pd_reg0, 0, 8, 8);

    // SAFETY: `preset` was constructed above from an aligned pointer attested valid for
    // `PRESET_BANK_SIZE` bytes, and `at` checks that each `PRESET*_REGISTER` offset is aligned
    // and within it.
    let (preset0, preset2) = unsafe { (preset.at(PRESET0_REGISTER), preset.at(PRESET2_REGISTER)) };

    // reset hdmi related blocks (HIU, HDMI SYS, HDMI_TX)
    preset0.write(1 << 19);
    preset2.write(1 << 15);
    preset2.write(1 << 2);

    let dwc_offset = (DWC_OFFSET_MASK >> 24) as usize;
    // SAFETY: `hdmitx` was constructed above from an aligned pointer attested valid for
    // `HDMITX_BANK_SIZE` bytes, and `at` checks that each TOP and DWC port offset is aligned and
    // within it.
    let (top, dwc): ([Reg32; 2], [Reg32; 2]) = unsafe {
        (
            [hdmitx.at(Offset::new(HDMITX_ADDR_PORT)), hdmitx.at(Offset::new(HDMITX_DATA_PORT))],
            [
                hdmitx.at(Offset::new(HDMITX_ADDR_PORT + dwc_offset)),
                hdmitx.at(Offset::new(HDMITX_DATA_PORT + dwc_offset)),
            ],
        )
    };

    // Bring HDMI out of reset
    hdmitx_writereg(top, dwc, HDMITX_TOP_SW_RESET, 0);
    spin_usecs(200);
    hdmitx_writereg(top, dwc, HDMITX_TOP_CLK_CNTL, 0x000000ff);
    hdmitx_writereg(top, dwc, HDMITX_DWC_MC_LOCKONCLOCK, 0xff);
    hdmitx_writereg(top, dwc, HDMITX_DWC_MC_CLKDIS, 0x00);

    // Configure HDCP
    let mut data32: u32 = 0;
    data32 |= 0 << 7;
    data32 |= 0 << 6;
    data32 |= 0 << 4;
    data32 |= 0 << 3;
    data32 |= 0 << 2;
    data32 |= 0 << 1;
    data32 |= 1 << 0;
    hdmitx_writereg(top, dwc, HDMITX_DWC_A_APIINTMSK, data32);

    data32 = 0;
    data32 |= 0 << 5;
    data32 |= 1 << 4;
    data32 |= 1 << 3;
    data32 |= 1 << 1;
    hdmitx_writereg(top, dwc, HDMITX_DWC_A_VIDPOLCFG, data32);

    hdmitx_writereg(top, dwc, HDMITX_DWC_A_OESSWCFG, 0x40);

    let _ = arm_smccc_smc(0x82000012, 0, 0, 0, 0, 0, 0, 0);
}

/// C-ABI entry point invoked during platform driver handoff.
///
/// # Safety
///
/// Same as [`amlogic_s912_hdcp_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn AmlogicS912HdcpInit(config: &DcfgAmlogicHdcpDriver) {
    // SAFETY: The caller upholds the requirements of `amlogic_s912_hdcp_init`.
    unsafe { amlogic_s912_hdcp_init(config) };
}

/// Amlogic S912 HDCP register helper unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "amlogic_s912_hdcp")]
mod tests {
    use unittest::assert_eq;

    /// Tests that `set_bit32` read-modify-writes only the selected field of the target register.
    #[test]
    fn test_set_bit32() {
        let mut regs = [0xffff_ffffu32, 0x0000_0000, 0xffff_ffff, 0xa5a5_a5a5, 0x0000_0000];
        let regs_size = size_of_val(&regs);
        let bank = MmioBank::new(
            // SAFETY: `regs` is a live, aligned local array of `regs_size` bytes that outlives
            // `bank` and is accessed only through `bank` until the asserts below, and, being
            // ordinary memory, accepts any written `u32` value.
            unsafe { MmioPtr::<u32, RwSafe>::new(regs.as_mut_ptr()) },
            regs_size,
        );
        // SAFETY: `bank` was constructed above from an aligned pointer attested valid for
        // `regs_size` bytes, and `at` checks that each offset is aligned and within it.
        let [reg0, reg1, reg2, reg4]: [Reg32; 4] = unsafe {
            [
                bank.at(Offset::new(0x0)),
                bank.at(Offset::new(0x4)),
                bank.at(Offset::new(0x8)),
                bank.at(Offset::new(0x10)),
            ]
        };

        set_bit32(reg0, 0x0100, 16, 0);
        set_bit32(reg1, 1, 1, 4);
        set_bit32(reg2, 0, 8, 8);
        // Bits of `value` outside the field are dropped.
        set_bit32(reg4, 0b11, 1, 4);

        assert_eq!(regs[0], 0xffff_0100);
        assert_eq!(regs[1], 0x0000_0010);
        assert_eq!(regs[2], 0xffff_00ff);
        assert_eq!(regs[3], 0xa5a5_a5a5);
        assert_eq!(regs[4], 0x0000_0010);
    }

    /// Tests that `hdmitx_writereg` routes TOP and DWC registers to their address and data ports.
    #[test]
    fn test_hdmitx_writereg() {
        let mut regs = [0u32; HDMITX_BANK_SIZE / 4];
        let bank = MmioBank::new(
            // SAFETY: `regs` is a live, aligned local array of `HDMITX_BANK_SIZE` bytes that
            // outlives `bank` and is accessed only through `bank` until the asserts below, and,
            // being ordinary memory, accepts any written `u32` value.
            unsafe { MmioPtr::<u32, RwSafe>::new(regs.as_mut_ptr()) },
            HDMITX_BANK_SIZE,
        );
        let dwc_offset = (DWC_OFFSET_MASK >> 24) as usize;
        // SAFETY: `bank` was constructed above from an aligned pointer attested valid for
        // `HDMITX_BANK_SIZE` bytes, and `at` checks that each TOP and DWC port offset is aligned
        // and within it.
        let (top, dwc): ([Reg32; 2], [Reg32; 2]) = unsafe {
            (
                [bank.at(Offset::new(HDMITX_ADDR_PORT)), bank.at(Offset::new(HDMITX_DATA_PORT))],
                [
                    bank.at(Offset::new(HDMITX_ADDR_PORT + dwc_offset)),
                    bank.at(Offset::new(HDMITX_DATA_PORT + dwc_offset)),
                ],
            )
        };

        hdmitx_writereg(top, dwc, HDMITX_TOP_CLK_CNTL, 0x000000ff);
        hdmitx_writereg(top, dwc, HDMITX_DWC_A_OESSWCFG, 0x40);

        assert_eq!(regs[0], 0x0001);
        assert_eq!(regs[1], 0x00ff);
        assert_eq!(regs[2], 0);
        assert_eq!(regs[3], 0);
        assert_eq!(regs[4], 0x500a);
        assert_eq!(regs[5], 0x0040);
    }
}
