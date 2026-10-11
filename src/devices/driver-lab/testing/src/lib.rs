// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test root driver for driver-lab driver realm tests.
//!
//! Adds one child node marked as a proxy target and offers it a fake
//! platform device with a single VMO-backed MMIO and virtual interrupt,
//! along with fake GPIO, I2C, and SPI protocol services, so the proxy
//! under test binds, maps real resources, and serves all protocol
//! operations the test can verify end to end.

use fake_pdev::FakePDev;
use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fidl_fuchsia_hardware_clock as fclock;
use fidl_fuchsia_hardware_gpio as fgpio;
use fidl_fuchsia_hardware_i2c as fi2c;
use fidl_fuchsia_hardware_reset as freset;
use fidl_fuchsia_hardware_serial as fserial;
use fidl_fuchsia_hardware_spi as fspi;
use fidl_next_fuchsia_hardware_platform_device as fdevice;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt, TryStreamExt};
use log::info;

/// Size of the fake MMIO region.
pub const MMIO_SIZE: u64 = 0x1000;
/// Seeded value at [`SEED_OFFSET`].
pub const SEED_VALUE: u32 = 0xFEED_FACE;
/// Offset of the seeded value.
pub const SEED_OFFSET: u64 = 0x10;

struct LabRootDriver {
    _node: Node,
    _scope: fasync::Scope,
    _virtual_irq: zx::VirtualInterrupt,
    _sample_virtual_irq: zx::VirtualInterrupt,
}

driver_register!(LabRootDriver);

fn make_mmio_vmo() -> Result<zx::Vmo, zx::Status> {
    let vmo = zx::Vmo::create(MMIO_SIZE)?;
    // The proxy maps this VMO with device cache policy; set it before
    // any pages are touched, then seed through a mapping (uncached VMOs
    // reject zx_vmo_write).
    vmo.set_cache_policy(zx::CachePolicy::UnCachedDevice)?;
    let mapping = mapped_vmo::Mapping::create_from_vmo(
        &vmo,
        MMIO_SIZE as usize,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    )?;
    mapping.write_at(SEED_OFFSET as usize, &SEED_VALUE.to_le_bytes());
    Ok(vmo)
}

async fn serve_fake_gpio(mut stream: fgpio::GpioRequestStream) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fgpio::GpioRequest::Read { responder } => {
                let _ = responder.send(Ok(true));
            }
            fgpio::GpioRequest::SetBufferMode { mode: _, responder } => {
                let _ = responder.send(Ok(()));
            }
            _ => {}
        }
    }
}

async fn serve_fake_i2c(mut stream: fi2c::DeviceRequestStream) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fi2c::DeviceRequest::Transfer { transactions, responder } => {
                if transactions.is_empty() {
                    let _ = responder.send(Err(zx::Status::INVALID_ARGS.into_raw()));
                } else {
                    let mut read_data: Vec<Vec<u8>> = Vec::new();
                    for txn in &transactions {
                        if let Some(fi2c::DataTransfer::ReadSize(size)) = &txn.data_transfer {
                            read_data.push(vec![0x42; *size as usize]);
                        }
                    }
                    let _ = responder.send(Ok(&read_data));
                }
            }
            _ => {}
        }
    }
}

async fn serve_fake_spi(mut stream: fspi::DeviceRequestStream) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fspi::DeviceRequest::CanAssertCs { responder } => {
                let _ = responder.send(true);
            }
            fspi::DeviceRequest::ExchangeVector { txdata, responder } => {
                let _ = responder.send(zx::sys::ZX_OK, &txdata);
            }
            _ => {}
        }
    }
}

async fn serve_fake_clock(mut stream: fclock::ClockRequestStream) {
    let mut enabled = false;
    let mut rate: u64 = 24_000_000;
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fclock::ClockRequest::Enable { responder } => {
                enabled = true;
                let _ = responder.send(Ok(()));
            }
            fclock::ClockRequest::Disable { responder } => {
                enabled = false;
                let _ = responder.send(Ok(()));
            }
            fclock::ClockRequest::IsEnabled { responder } => {
                let _ = responder.send(Ok(enabled));
            }
            fclock::ClockRequest::SetRate { hz, responder } => {
                rate = hz;
                let _ = responder.send(Ok(()));
            }
            fclock::ClockRequest::QuerySupportedRate { hz_in, responder } => {
                let _ = responder.send(Ok(hz_in));
            }
            fclock::ClockRequest::GetRate { responder } => {
                let _ = responder.send(Ok(rate));
            }
            _ => {}
        }
    }
}

async fn serve_fake_reset(mut stream: freset::ResetRequestStream) {
    let mut asserted = false;
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            freset::ResetRequest::Assert { responder } => {
                asserted = true;
                let _ = responder.send(Ok(()));
            }
            freset::ResetRequest::Deassert { responder } => {
                asserted = false;
                let _ = responder.send(Ok(()));
            }
            freset::ResetRequest::Toggle { responder } => {
                asserted = !asserted;
                let _ = responder.send(Ok(()));
            }
            freset::ResetRequest::Status { responder } => {
                let _ = responder.send(Ok(asserted));
            }
            _ => {}
        }
    }
}

async fn serve_fake_serial(mut stream: fserial::DeviceRequestStream) {
    let mut buffer: Vec<u8> = Vec::new();
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fserial::DeviceRequest::GetClass { responder } => {
                let _ = responder.send(fserial::Class::Generic);
            }
            fserial::DeviceRequest::Read { responder } => {
                let data = std::mem::take(&mut buffer);
                let _ = responder.send(Ok(&data));
            }
            fserial::DeviceRequest::Write { data, responder } => {
                buffer.extend_from_slice(&data);
                let _ = responder.send(Ok(()));
            }
            _ => {}
        }
    }
}

impl Driver for LabRootDriver {
    const NAME: &str = "lab_root";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;

        let vmo = make_mmio_vmo().map_err(DriverError::Status)?;
        let mut config = fake_pdev::Config::default();
        config.mmios.insert(
            0,
            fdevice::natural::Mmio { offset: Some(0), size: Some(MMIO_SIZE), vmo: Some(vmo) },
        );

        let virtual_irq = zx::VirtualInterrupt::create_virtual().map_err(DriverError::Status)?;
        let client_irq = zx::Interrupt::from(
            virtual_irq
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .map_err(DriverError::Status)?
                .into_handle(),
        );
        config.irqs.insert(0, client_irq);

        config.device_info = Some(fdevice::natural::NodeDeviceInfo {
            mmio_count: Some(1),
            irq_count: Some(1),
            ..Default::default()
        });
        let pdev = FakePDev::new();
        pdev.set_config(config);

        let scope = fasync::Scope::new_with_name(Self::NAME);
        let mut fs = ServiceFs::new();
        let offer = pdev.serve(&mut fs, scope.to_handle(), "pdev");

        let gpio_offer = ServiceOffer::new_marker(fgpio::ServiceMarker).build_zircon_offer();
        let i2c_offer = ServiceOffer::new_marker(fi2c::ServiceMarker).build_zircon_offer();
        let spi_offer = ServiceOffer::new_marker(fspi::ServiceMarker).build_zircon_offer();
        let clock_offer = ServiceOffer::new_marker(fclock::ServiceMarker).build_zircon_offer();
        let reset_offer = ServiceOffer::new_marker(freset::ServiceMarker).build_zircon_offer();
        let serial_offer = ServiceOffer::new_marker(fserial::ServiceMarker).build_zircon_offer();

        let handle = scope.to_handle();
        let h_gpio = handle.clone();
        fs.dir("svc").add_fidl_service_instance(
            "default",
            move |request: fgpio::ServiceRequest| {
                let fgpio::ServiceRequest::Device(stream) = request;
                h_gpio.spawn(serve_fake_gpio(stream));
            },
        );

        let h_i2c = handle.clone();
        fs.dir("svc").add_fidl_service_instance("default", move |request: fi2c::ServiceRequest| {
            let fi2c::ServiceRequest::Device(stream) = request;
            h_i2c.spawn(serve_fake_i2c(stream));
        });

        let h_spi = handle.clone();
        fs.dir("svc").add_fidl_service_instance("default", move |request: fspi::ServiceRequest| {
            let fspi::ServiceRequest::Device(stream) = request;
            h_spi.spawn(serve_fake_spi(stream));
        });

        let h_clock = handle.clone();
        fs.dir("svc").add_fidl_service_instance(
            "default",
            move |request: fclock::ServiceRequest| {
                let fclock::ServiceRequest::Clock(stream) = request;
                h_clock.spawn(serve_fake_clock(stream));
            },
        );

        let h_reset = handle.clone();
        fs.dir("svc").add_fidl_service_instance(
            "default",
            move |request: freset::ServiceRequest| {
                let freset::ServiceRequest::Reset(stream) = request;
                h_reset.spawn(serve_fake_reset(stream));
            },
        );

        let h_serial = handle.clone();
        fs.dir("svc").add_fidl_service_instance(
            "default",
            move |request: fserial::ServiceRequest| {
                let fserial::ServiceRequest::Device(stream) = request;
                h_serial.spawn(serve_fake_serial(stream));
            },
        );

        let sample_vmo = make_mmio_vmo().map_err(DriverError::Status)?;
        let mut sample_config = fake_pdev::Config::default();
        sample_config.mmios.insert(
            0,
            fdevice::natural::Mmio {
                offset: Some(0),
                size: Some(MMIO_SIZE),
                vmo: Some(sample_vmo),
            },
        );
        let sample_virtual_irq =
            zx::VirtualInterrupt::create_virtual().map_err(DriverError::Status)?;
        let sample_client_irq = zx::Interrupt::from(
            sample_virtual_irq
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .map_err(DriverError::Status)?
                .into_handle(),
        );
        sample_config.irqs.insert(0, sample_client_irq);
        sample_config.device_info = Some(fdevice::natural::NodeDeviceInfo {
            mmio_count: Some(1),
            irq_count: Some(1),
            ..Default::default()
        });
        let sample_pdev = FakePDev::new();
        sample_pdev.set_config(sample_config);
        let sample_offer = sample_pdev.serve(&mut fs, scope.to_handle(), "sample-pdev");

        context.serve_outgoing(&mut fs)?;
        scope.spawn(async move {
            fs.collect::<()>().await;
        });

        let virtual_irq_clone =
            virtual_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(DriverError::Status)?;
        let sample_irq_clone = sample_virtual_irq
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .map_err(DriverError::Status)?;
        scope.spawn(async move {
            loop {
                fasync::Timer::new(fasync::MonotonicInstant::after(zx::Duration::from_millis(50)))
                    .await;
                let _ = virtual_irq_clone.trigger(zx::BootInstant::from_nanos(0));
                let _ = sample_irq_clone.trigger(zx::BootInstant::from_nanos(0));
            }
        });

        let child = NodeBuilder::new("proxy-target")
            .add_property(bind_fuchsia_driver_lab::PROXY_TARGET, true)
            .add_property(bind_fuchsia::SERVICE, "fuchsia.hardware.platform.device.Service")
            .add_offer(offer)
            .add_offer(gpio_offer)
            .add_offer(i2c_offer)
            .add_offer(spi_offer)
            .add_offer(clock_offer)
            .add_offer(reset_offer)
            .add_offer(serial_offer)
            .build();
        node.add_child(child).await?;

        let sample_child = NodeBuilder::new("sample-device")
            .add_property(bind_fuchsia_driver_lab::SAMPLE_DEVICE, true)
            .add_property(bind_fuchsia::SERVICE, "fuchsia.hardware.platform.device.Service")
            .add_offer(sample_offer)
            .build();
        node.add_child(sample_child).await?;

        info!("lab_root added proxy-target and sample-device nodes with MMIO, IRQ, GPIO, I2C, SPI");
        Ok(Self {
            _node: node,
            _scope: scope,
            _virtual_irq: virtual_irq,
            _sample_virtual_irq: sample_virtual_irq,
        })
    }

    async fn stop(&self) {}
}
