// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_adcimpl as adcimpl;
use fidl_next_fuchsia_hardware_adcimpl::device::{GetResolution, GetSample};
use fuchsia_async::{self as fasync, TimeoutExt};
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use futures::channel::mpsc;
use log::{error, warn};
use pdev::{PdevExt, PlatformDevice};
use std::pin::Pin;
use zx::Status;

use fdf_metadata::MetadataServer;

use mmio::region::MmioRegion;
use mmio::vmo::VmoMemory;
mod registers;
use registers::*;

const MAX_CHANNELS: u8 = 8;
const SAR_ADC_RESOLUTION: u8 = 10;
/// Width of a sample in FIFO_RD.
const SAMPLE_BITS: u8 = 12;
/// Number of entries in the sample FIFO.
const FIFO_SIZE: u32 = 32;
/// How long to wait for a sample from the requested channel before giving up.
const SAMPLE_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(20);
/// How long to wait for a conversion in progress to finish after sampling is stopped.
const STOP_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(10);

const CLK_SRC_OSCIN: u32 = 0;

struct AmlSaradcDevice<K: zx::InterruptKind> {
    adc_regs: AdcRegsBlock<MmioRegion<VmoMemory>>,
    ao_regs: AoRegsBlock<MmioRegion<VmoMemory>>,
    /// The FIFO interrupt. It can only be bound to a port once, so it's bound on first use and
    /// must be acked after each delivery. It's edge-triggered, firing when the FIFO becomes
    /// non-empty.
    irq: Pin<Box<fasync::OnInterrupt<K>>>,
    /// Set if waiting for the interrupt failed, e.g. because it couldn't be bound. All later
    /// requests fail with this error, since the interrupt can't be waited on again.
    irq_error: Option<Status>,
    /// Number of consecutive requests that timed out, to avoid logging every one.
    consecutive_timeouts: u32,
}

struct AmlSaradc {
    _node: Node,
    tx: mpsc::UnboundedSender<AdcRequest>,
    _scope: fasync::Scope,
}

driver_register!(AmlSaradc);

impl<K: zx::InterruptKind> AmlSaradcDevice<K> {
    fn new(
        adc_regs: AdcRegsBlock<MmioRegion<VmoMemory>>,
        ao_regs: AoRegsBlock<MmioRegion<VmoMemory>>,
        irq: zx::Interrupt<K>,
    ) -> Self {
        Self {
            adc_regs,
            ao_regs,
            irq: Box::pin(fasync::OnInterrupt::new(irq)),
            irq_error: None,
            consecutive_timeouts: 0,
        }
    }

    fn set_clock(&mut self, src: u32, div: u32) {
        self.ao_regs.ao_sar_clk_mut().update(|reg| {
            reg.set_clk_src(src);
            reg.set_clk_div(div);
        });
    }

    fn clk_ena(&mut self, ena: bool) {
        self.ao_regs.ao_sar_clk_mut().update(|reg| {
            reg.set_clk_ena(ena);
        });
    }

    async fn enable(&mut self, ena: bool) {
        if ena {
            self.adc_regs.reg11_mut().update(|reg| {
                // Enable bandgap reference
                reg.set_ts_vbg_en(true);
                // Set common mode vref
                reg.set_rsv6(false);
                // Select bandgap as reference
                reg.set_rsv5(false);
            });
            self.adc_regs.reg0_mut().update(|reg| {
                // Enable IRQ
                reg.set_fifo_irq_en(true);
            });
            self.adc_regs.reg3_mut().update(|reg| {
                // Enable the ADC
                reg.set_adc_en(true);
            });
            fuchsia_async::Timer::new(std::time::Duration::from_micros(5)).await;
            self.clk_ena(true);
        } else {
            self.adc_regs.reg0_mut().update(|reg| {
                // Disable IRQ
                reg.set_fifo_irq_en(false);
            });
            self.clk_ena(false);
            self.adc_regs.reg3_mut().update(|reg| {
                // Disable the ADC
                reg.set_adc_en(false);
            });
        }
        fuchsia_async::Timer::new(std::time::Duration::from_micros(10)).await;
    }

    fn stop(&mut self) {
        self.adc_regs.reg0_mut().update(|reg| {
            // Stop Conversion
            reg.set_sampling_stop(true);
            // Disable Sampling
            reg.set_sampling_enable(false);
        });
    }

    async fn hw_init(&mut self) {
        {
            let adc_regs = &mut self.adc_regs;
            adc_regs.reg0_mut().write({
                let mut reg = Reg0::default();
                reg.set_val(0x84004040);
                reg
            });
            adc_regs.reg0_mut().update(|reg| {
                // Set IRQ trigger to one sample.
                reg.set_fifo_cnt_irq(1);
            });

            // Set channel list to only channel zero
            adc_regs.chan_list_mut().write(ChanList(0x00000000));
            // Disable averaging modes
            adc_regs.avg_cntl_mut().write(AvgCntl(0x00000000));
            adc_regs.reg3_mut().write({
                let mut reg = Reg3::default();
                reg.set_val(0x9388000a);
                reg
            });
            adc_regs.delay_mut().write(Delay(0x010a000a));
            adc_regs.aux_sw_mut().write(AuxSw(0x03eb1a0c));
            adc_regs.chan_10_sw_mut().write(Chan10Sw(0x008c000c));
            adc_regs.detect_idle_sw_mut().write(DetectIdleSw(0x000c000c));

            // Disable ring counter (not used on g12)
            adc_regs.reg3_mut().update(|reg| {
                let mut val = reg.val();
                val |= 1 << 27;
                reg.set_val(val);
            });

            adc_regs.reg11_mut().update(|reg| {
                reg.set_rsv1(true);
            });

            adc_regs.reg13_mut().write(Reg13(0x00002000));
        }

        self.set_clock(CLK_SRC_OSCIN, 20);
        self.enable(true).await;
        fuchsia_async::Timer::new(std::time::Duration::from_micros(10)).await;
    }

    /// Discards all samples in the FIFO, so that stale samples from a previous request aren't
    /// returned.
    fn drain_fifo(&mut self) {
        for _ in 0..FIFO_SIZE {
            if self.adc_regs.reg0().read().fifo_empty() {
                break;
            }
            let _ = self.adc_regs.fifo_rd().read();
        }
    }

    /// Reads samples from the FIFO until one from `channel` is found, discarding samples from
    /// other channels. Returns `None` if the FIFO has no sample from `channel`.
    fn read_fifo_sample(&mut self, channel: u8) -> Option<u32> {
        for _ in 0..FIFO_SIZE {
            if self.adc_regs.reg0().read().fifo_empty() {
                return None;
            }
            let entry = self.adc_regs.fifo_rd().read();
            if entry.chan_id() == channel {
                // Report the top `SAR_ADC_RESOLUTION` bits of the sample.
                return Some(entry.sample() >> (SAMPLE_BITS - SAR_ADC_RESOLUTION));
            }
        }
        None
    }

    /// Waits for any conversion in progress to finish, after sampling has been stopped.
    fn wait_until_idle(&mut self) {
        let deadline = zx::MonotonicInstant::after(STOP_TIMEOUT);
        while self.adc_regs.reg0().read().busy() != 0 {
            if zx::MonotonicInstant::get() >= deadline {
                warn!("Timed out waiting for the ADC to stop");
                return;
            }
            std::hint::spin_loop();
        }
    }

    /// Waits for a sample from `channel`, until `SAMPLE_TIMEOUT` elapses.
    async fn wait_for_sample(&mut self, channel: u8) -> Result<u32, Status> {
        let deadline = fasync::MonotonicInstant::after(SAMPLE_TIMEOUT);
        loop {
            if let Some(sample) = self.read_fifo_sample(channel) {
                self.consecutive_timeouts = 0;
                return Ok(sample);
            }
            // `OnInterrupt` never ends, so `None` means that the deadline passed.
            match self.irq.next().on_timeout(deadline, || None).await {
                Some(Ok(_)) => {
                    // Re-arm the interrupt so that the next one is delivered. (`Pin::as_ref`
                    // shadows `AsRef::as_ref`, hence the explicit type.)
                    let irq: &zx::Interrupt<K> = (*self.irq).as_ref();
                    irq.ack().inspect_err(|status| {
                        error!("Failed to ack the ADC interrupt: {status}");
                    })?;
                }
                Some(Err(status)) => {
                    error!("Failed to wait for the ADC interrupt: {status}");
                    self.irq_error = Some(status);
                    return Err(status);
                }
                None => {
                    // The interrupt only fires when the FIFO becomes non-empty, so a sample that
                    // arrived without one would otherwise never be read. Check once more.
                    let sample = self.read_fifo_sample(channel);
                    self.consecutive_timeouts += 1;
                    let level = if self.consecutive_timeouts == 1 {
                        log::Level::Warn
                    } else {
                        log::Level::Debug
                    };
                    return match sample {
                        Some(sample) => {
                            log::log!(
                                level,
                                "Got a sample from channel {channel} without an interrupt"
                            );
                            Ok(sample)
                        }
                        None => {
                            log::log!(
                                level,
                                "Timed out waiting for a sample from channel {channel}"
                            );
                            Err(Status::TIMED_OUT)
                        }
                    };
                }
            }
        }
    }

    async fn get_sample(&mut self, channel: u8) -> Result<u32, Status> {
        if channel >= MAX_CHANNELS {
            return Err(Status::INVALID_ARGS);
        }
        if let Some(status) = self.irq_error {
            return Err(status);
        }

        self.clk_ena(false);
        self.set_clock(CLK_SRC_OSCIN, 160);
        self.clk_ena(true);

        {
            let adc_regs = &mut self.adc_regs;
            adc_regs.chan_list_mut().write(ChanList(channel as u32));

            adc_regs.detect_idle_sw_mut().write(DetectIdleSw(
                0x000c000c | ((channel as u32) << 23) | ((channel as u32) << 7),
            ));

            adc_regs.reg0_mut().update(|reg| {
                reg.set_sampling_enable(true);
            });
        }

        // Discard samples left over from a previous request before starting a new conversion.
        self.drain_fifo();

        self.adc_regs.reg0_mut().update(|reg| {
            reg.set_sampling_start(true);
        });

        let result = self.wait_for_sample(channel).await;

        // Stop sampling, wait for any conversion in progress to finish, and discard any extra
        // samples before disabling sampling.
        self.adc_regs.reg0_mut().update(|reg| {
            reg.set_sampling_stop(true);
        });
        self.wait_until_idle();
        self.drain_fifo();
        self.adc_regs.reg0_mut().update(|reg| {
            reg.set_sampling_enable(false);
        });

        self.clk_ena(false);
        self.set_clock(CLK_SRC_OSCIN, 20);
        self.clk_ena(true);

        result
    }
}

enum AdcRequest {
    GetSample { channel: u8, responder: Responder<GetSample> },
    Stop,
}

struct DeviceServer {
    tx: mpsc::UnboundedSender<AdcRequest>,
}

impl adcimpl::DeviceLocalServerHandler for DeviceServer {
    async fn get_resolution(&mut self, responder: Responder<GetResolution>) {
        responder
            .respond(SAR_ADC_RESOLUTION)
            .await
            .unwrap_or_else(|err| error!("Failed to send get_resolution response: {err:?}"));
    }

    async fn get_sample(&mut self, request: Request<GetSample>, responder: Responder<GetSample>) {
        let channel_id = request.payload().channel_id;
        if let Err(e) =
            self.tx.unbounded_send(AdcRequest::GetSample { channel: channel_id as u8, responder })
        {
            error!("Failed to send message to actor: {e:?}");
            let req = e.into_inner();
            let responder = match req {
                AdcRequest::GetSample { responder, .. } => responder,
                _ => unreachable!(),
            };
            let _ = responder.respond_err(Status::INTERNAL).await;
        }
    }
}

struct Service {
    tx: mpsc::UnboundedSender<AdcRequest>,
    scope: fasync::ScopeHandle,
}

impl adcimpl::ServiceHandler for Service {
    fn device(&self, server_end: ServerEnd<adcimpl::Device>) {
        let tx = self.tx.clone();
        self.scope.spawn_local(async move {
            let dispatcher = fidl_next::ServerDispatcher::new(server_end);
            let _ = dispatcher.run_local(DeviceServer { tx }).await;
        });
    }
}

impl Driver for AmlSaradc {
    const NAME: &str = "aml-saradc";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;

        let pdev = context.connect_to_pdev()?;

        let adc_mmio = pdev.map_mmio_by_id(0).await?;
        let ao_mmio = pdev.map_mmio_by_id(1).await?;

        let irq = pdev.get_interrupt_by_id(0, 0).await??.irq;

        let mut device =
            AmlSaradcDevice::new(AdcRegsBlock::new(adc_mmio), AoRegsBlock::new(ao_mmio), irq);

        device.hw_init().await;

        let (tx, mut rx) = mpsc::unbounded::<AdcRequest>();

        let scope = fasync::Scope::new_with_name("driver");

        let metadata_server = MetadataServer::new("fuchsia.hardware.adcimpl.Metadata")
            .forward_from_pdev_next(&pdev)
            .await?;

        let mut outgoing = ServiceFs::new();
        let offer = ServiceOffer::<adcimpl::Service>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                Service { tx: tx.clone(), scope: scope.to_handle() },
            )
            .build_driver_offer();

        let mut child_builder = NodeBuilder::new("aml-saradc").add_offer(offer);

        if let Some(offer) = metadata_server.serve(&mut outgoing, scope.to_handle(), "default") {
            child_builder = child_builder.add_offer(offer);
        }

        let child_node = child_builder.build();
        node.add_child(child_node).await?;

        context.serve_outgoing(&mut outgoing)?;
        scope.spawn(outgoing.collect());

        let mut device_for_task = device;
        scope.spawn_local(async move {
            while let Some(req) = rx.next().await {
                match req {
                    AdcRequest::GetSample { channel, responder } => {
                        let result = device_for_task.get_sample(channel).await;
                        match result {
                            Ok(value) => {
                                let _ = responder.respond(value).await;
                            }
                            Err(status) => {
                                let _ = responder.respond_err(status).await;
                            }
                        }
                    }
                    AdcRequest::Stop => {
                        device_for_task.stop();
                        device_for_task.enable(false).await;
                        break;
                    }
                }
            }
        });

        Ok(AmlSaradc { _node: node, tx, _scope: scope })
    }

    async fn stop(&self) {
        let _ = self.tx.unbounded_send(AdcRequest::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmio::vmo::VmoMapping;
    use std::pin::pin;
    use std::task::Poll;
    use zx::Vmo;

    #[fuchsia::test]
    async fn test_get_resolution() {
        assert_eq!(SAR_ADC_RESOLUTION, 10);
    }

    const REG0_OFFSET: u64 = 0x00;
    const FIFO_RD_OFFSET: u64 = 0x18;
    // `Reg0::fifo_empty`.
    const REG0_FIFO_EMPTY: u32 = 1 << 26;

    struct TestDevice {
        device: AmlSaradcDevice<zx::VirtualInterruptKind>,
        adc_vmo: Vmo,
        irq: zx::Interrupt<zx::VirtualInterruptKind>,
    }

    // Creates a device whose FIFO holds `fifo_entry`, or is empty if `fifo_entry` is `None`.
    // TODO(b/581080223): Use a fake MMIO whose FIFO is consumed by reads, to also test draining.
    fn create_device(fifo_entry: Option<u32>) -> TestDevice {
        let adc_vmo = Vmo::create(1024).unwrap();
        let ao_vmo = Vmo::create(1024).unwrap();
        let irq = zx::Interrupt::create_virtual().unwrap();
        let test_device = TestDevice {
            device: AmlSaradcDevice::new(
                // The test updates the fake registers through `adc_vmo`, which requires a cached
                // mapping.
                AdcRegsBlock::new(
                    VmoMapping::map_with_cache_policy(
                        0,
                        1024,
                        adc_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
                        zx::CachePolicy::Cached,
                    )
                    .unwrap(),
                ),
                AoRegsBlock::new(VmoMapping::map(0, 1024, ao_vmo).unwrap()),
                irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            ),
            adc_vmo,
            irq,
        };
        set_fifo(&test_device.adc_vmo, fifo_entry);
        test_device
    }

    // Sets the FIFO to hold `fifo_entry`, or to be empty if `fifo_entry` is `None`.
    fn set_fifo(adc_vmo: &Vmo, fifo_entry: Option<u32>) {
        let mut reg0 = [0u8; 4];
        adc_vmo.read(&mut reg0, REG0_OFFSET).unwrap();
        let reg0 = u32::from_le_bytes(reg0);
        let reg0 =
            if fifo_entry.is_some() { reg0 & !REG0_FIFO_EMPTY } else { reg0 | REG0_FIFO_EMPTY };
        adc_vmo.write(&reg0.to_le_bytes(), REG0_OFFSET).unwrap();
        adc_vmo.write(&fifo_entry.unwrap_or(0).to_le_bytes(), FIFO_RD_OFFSET).unwrap();
    }

    // Returns a FIFO entry for `channel` with a 10-bit `sample`, which the hardware reports in the
    // top bits of a 12-bit sample.
    fn fifo_entry(channel: u32, sample: u32) -> u32 {
        (channel << 12) | (sample << (SAMPLE_BITS - SAR_ADC_RESOLUTION))
    }

    // Returns a fake-time executor. If `allow_interrupts`, its port can be bound to interrupts, as
    // the driver's is.
    fn executor(allow_interrupts: bool) -> fasync::TestExecutor {
        let executor = fasync::TestExecutor::builder()
            .fake_time(true)
            .allow_interrupts(allow_interrupts)
            .build();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));
        executor
    }

    // Simulates a conversion of `channel` completing: the sample is put in the FIFO, then the
    // interrupt fires.
    fn complete_conversion(
        adc_vmo: &Vmo,
        irq: &zx::Interrupt<zx::VirtualInterruptKind>,
        channel: u32,
        sample: u32,
    ) {
        set_fifo(adc_vmo, Some(fifo_entry(channel, sample)));
        irq.trigger(zx::BootInstant::get()).unwrap();
    }

    // Advances fake time past `SAMPLE_TIMEOUT`.
    fn time_out(executor: &mut fasync::TestExecutor) {
        executor.set_fake_time(fasync::MonotonicInstant::after(SAMPLE_TIMEOUT));
        executor.wake_expired_timers();
    }

    #[fuchsia::test]
    fn test_get_sample() {
        let mut executor = executor(true);
        let TestDevice { mut device, adc_vmo, irq } = create_device(None);
        let mut request = pin!(device.get_sample(2));
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        complete_conversion(&adc_vmo, &irq, 2, 665);
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Ready(Ok(665)));
    }

    #[fuchsia::test]
    fn test_get_sample_consecutive_channels() {
        // Each request waits for its own conversion instead of returning the previous sample.
        let mut executor = executor(true);
        let TestDevice { mut device, adc_vmo, irq } = create_device(None);
        for (channel, sample) in [(2, 665), (1, 668), (2, 664)] {
            let mut request = pin!(device.get_sample(channel as u8));
            assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
            complete_conversion(&adc_vmo, &irq, channel, sample);
            assert_eq!(executor.run_until_stalled(&mut request), Poll::Ready(Ok(sample)));
            set_fifo(&adc_vmo, None);
        }
    }

    #[fuchsia::test]
    fn test_get_sample_ignores_samples_from_other_channels() {
        let mut executor = executor(true);
        let TestDevice { mut device, adc_vmo, irq } = create_device(None);
        let mut request = pin!(device.get_sample(1));
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        complete_conversion(&adc_vmo, &irq, 2, 665);
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        complete_conversion(&adc_vmo, &irq, 1, 668);
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Ready(Ok(668)));
    }

    #[fuchsia::test]
    fn test_get_sample_ignores_spurious_interrupts() {
        let mut executor = executor(true);
        let TestDevice { mut device, adc_vmo, irq } = create_device(None);
        let mut request = pin!(device.get_sample(2));
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        irq.trigger(zx::BootInstant::get()).unwrap();
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        // The interrupt was acked, so the next one is delivered.
        complete_conversion(&adc_vmo, &irq, 2, 665);
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Ready(Ok(665)));
    }

    #[fuchsia::test]
    fn test_get_sample_times_out_without_samples() {
        let mut executor = executor(true);
        let TestDevice { mut device, .. } = create_device(None);
        let mut request = pin!(device.get_sample(1));
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        time_out(&mut executor);
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Ready(Err(Status::TIMED_OUT)));
    }

    #[fuchsia::test]
    fn test_get_sample_without_interrupt() {
        // A sample that arrives without an interrupt is still returned when the request times out.
        let mut executor = executor(true);
        let TestDevice { mut device, adc_vmo, .. } = create_device(None);
        let mut request = pin!(device.get_sample(2));
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        set_fifo(&adc_vmo, Some(fifo_entry(2, 665)));
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Pending);
        time_out(&mut executor);
        assert_eq!(executor.run_until_stalled(&mut request), Poll::Ready(Ok(665)));
    }

    #[fuchsia::test]
    fn test_get_sample_fails_if_interrupt_cannot_be_waited_on() {
        // The interrupt can't be bound to a port without `BIND_TO_INTERRUPT`. This logs an error,
        // which is allowed by the test package's `max_severity`.
        let mut executor = executor(false);
        let TestDevice { mut device, .. } = create_device(None);
        assert_eq!(
            executor.run_until_stalled(&mut pin!(device.get_sample(2))),
            Poll::Ready(Err(Status::WRONG_TYPE))
        );
        // Later requests fail immediately with the same error.
        assert_eq!(
            executor.run_until_stalled(&mut pin!(device.get_sample(2))),
            Poll::Ready(Err(Status::WRONG_TYPE))
        );
    }

    #[fuchsia::test]
    async fn test_get_sample_invalid_args() {
        let TestDevice { mut device, .. } = create_device(None);
        assert_eq!(device.get_sample(MAX_CHANNELS).await, Err(Status::INVALID_ARGS));
    }
}
