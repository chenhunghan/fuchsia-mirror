// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fastboot_interface::{
    Fastboot, FastbootError, FastbootInterface, FlashError, RebootEvent, StageError,
    UploadProgress, Variable,
};
use crate::interface_factory::InterfaceFactory;
use crate::stream::{StreamCommand, StreamOp};
use anyhow::{Result, anyhow};
use chrono::Duration;
use fastboot::UploadError::*;
use fastboot::command::{ClientVariable, Command};
use fastboot::reply::Reply;
use fastboot::{
    BUFFER_SIZE, FastbootContext, UploadProgressListener, download, read_and_log_info_with_timeout,
    send, send_with_listener, send_with_timeout, upload, upload_from_reader,
    upload_with_read_timeout,
};
use ffx_futures::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use std::fmt::Debug;
use std::fs::File;
use tokio::sync::mpsc::Sender;

///////////////////////////////////////////////////////////////////////////////
// FastbootProxy
//

#[derive(Debug)]
pub struct FastbootProxy<T: AsyncRead + AsyncWrite + Unpin + Send, F: InterfaceFactory<T>> {
    interface: Option<T>,
    interface_factory: F,
    ctx: FastbootContext,
}

fn handle_timeout_as_okay(
    r: Result<Reply, fastboot::FastbootError>,
) -> Result<Reply, fastboot::FastbootError> {
    match r {
        Err(fastboot::FastbootError::Read(fastboot::ReadError::Timeout)) => {
            log::debug!("Timed out waiting for bootloader response; assuming it's okay");
            Ok(Reply::Okay("".to_string()))
        }
        _ => r,
    }
}

impl<T, F> FastbootInterface for FastbootProxy<T, F>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send + 'static,
    F: InterfaceFactory<T> + 'static,
{
}

#[derive(Debug)]
struct VariableListener(Sender<Variable>);

impl VariableListener {
    fn new(listener: Sender<Variable>) -> Self {
        Self(listener)
    }
}

impl fastboot::InfoListener for VariableListener {
    async fn on_info(&self, info: String) -> Result<(), fastboot::ReadError> {
        if let Some((name, val)) = info.split_once(':') {
            log::debug!("Got a variable string: {}", info);
            self.0
                .send(Variable { name: name.to_string(), value: val.to_string() })
                .await
                .map_err(|e| {
                    fastboot::ReadError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
                })?;
        } else {
            log::warn!("Expected to get a variable string. Got: {}", info);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ProgressListener<'a>(&'a Sender<UploadProgress>);

impl<'a> ProgressListener<'a> {
    fn new(listener: &'a Sender<UploadProgress>) -> Self {
        Self(listener)
    }
}

impl fastboot::UploadProgressListener for ProgressListener<'_> {
    async fn on_started(&self, size: usize) -> Result<(), fastboot::UploadError> {
        let size = size.try_into().map_err(|e| {
            fastboot::UploadError::CouldNotVerifyUpload(fastboot::ReadError::Io(
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            ))
        })?;
        self.0.send(UploadProgress::OnStarted { size }).await.map_err(|e| {
            fastboot::UploadError::CouldNotWriteToInterface(std::io::Error::new(
                std::io::ErrorKind::Other,
                e,
            ))
        })?;
        Ok(())
    }
    async fn on_progress(&self, bytes_written: u64) -> Result<(), fastboot::UploadError> {
        self.0.send(UploadProgress::OnProgress { bytes_written }).await.map_err(|e| {
            fastboot::UploadError::CouldNotWriteToInterface(std::io::Error::new(
                std::io::ErrorKind::Other,
                e,
            ))
        })?;
        Ok(())
    }
    async fn on_error(&self, error: &fastboot::UploadError) -> Result<(), fastboot::UploadError> {
        self.0.send(UploadProgress::OnError { error: anyhow!(error.to_string()) }).await.map_err(
            |e| {
                fastboot::UploadError::CouldNotWriteToInterface(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e,
                ))
            },
        )?;
        Ok(())
    }
    async fn on_finished(&self) -> Result<(), fastboot::UploadError> {
        self.0.send(UploadProgress::OnFinished).await.map_err(|e| {
            fastboot::UploadError::CouldNotWriteToInterface(std::io::Error::new(
                std::io::ErrorKind::Other,
                e,
            ))
        })?;
        Ok(())
    }
}

impl<T, F> FastbootProxy<T, F>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
    F: InterfaceFactory<T>,
{
    pub fn new(_target_id: String, interface: T, interface_factory: F) -> Self {
        Self { interface: Some(interface), interface_factory, ctx: FastbootContext::new() }
    }

    async fn reconnect(&mut self) -> Result<(), crate::interface_factory::InterfaceFactoryError> {
        // Explicitly here.
        self.interface = None;

        // Wait for it to show up again
        log::debug!("About to rediscover target");
        self.interface_factory.rediscover().await?;

        //Reconnect
        self.interface.replace(self.interface_factory.open().await?);
        log::warn!("Reconnected");
        Ok(())
    }

    async fn interface(
        &mut self,
    ) -> Result<&mut T, crate::interface_factory::InterfaceFactoryError> {
        if self.interface.is_none() {
            self.interface.replace(self.interface_factory.open().await?);
        }
        Ok(self.interface.as_mut().expect("interface interface not available"))
    }

    fn invalidate_on_transport_error<R>(
        &mut self,
        res: Result<R, FastbootError>,
    ) -> Result<R, FastbootError> {
        if let Err(ref err) = res
            && is_proxy_transport_error(err)
        {
            log::warn!("Invalidating cached fastboot interface after transport error: {err}");
            self.interface = None;
        }
        res
    }

    async fn flash_uploaded(
        &mut self,
        partition_name: &str,
        timeout: Duration,
    ) -> Result<(), FastbootError> {
        // Flash the uploaded file
        let command = Command::Flash(partition_name.to_string());
        let send_reply =
            send_with_timeout(self.ctx.clone(), command.clone(), self.interface().await?, timeout)
                .await;
        match send_reply {
            Ok(reply) => match reply {
                Reply::Okay(_) => Ok(()),
                Reply::Fail(s) => Err(FastbootError::FlashError(FlashError::FlashFailed {
                    partition: partition_name.to_string(),
                    message: s,
                })),
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: command.to_string(),
                    reply: r.to_string(),
                }),
            },
            Err(ref e) => {
                self.interface = None;
                let e: &fastboot::FastbootError = e;
                if let fastboot::FastbootError::Read(fastboot::ReadError::Timeout) = e {
                    let message = format!(
                        "Time out while waiting on a response from the device. \n\
                            The current timeout is {}.  Try increasing the timeout",
                        timeout
                    );
                    Err(FastbootError::FlashError(FlashError::TimeoutError(message)))
                } else {
                    Err(FastbootError::FlashError(FlashError::Error(anyhow::anyhow!(
                        e.to_string()
                    ))))
                }
            }
        }
    }
}

fn is_fastboot_transport_error(err: &fastboot::FastbootError) -> bool {
    match err {
        fastboot::FastbootError::Io(_) => true,
        fastboot::FastbootError::Read(
            fastboot::ReadError::Io(_)
            | fastboot::ReadError::Timeout
            | fastboot::ReadError::Parse(fastboot::reply::ParseReplyError::ReplyTooShort {
                reply_len: 0,
            }),
        ) => true,
        fastboot::FastbootError::Upload(CouldNotWriteToInterface(_) | CouldNotVerifyUpload(_)) => {
            true
        }
        fastboot::FastbootError::Download(
            fastboot::DownloadError::CouldNotReadToInterface(_)
            | fastboot::DownloadError::CouldNotVerifyDownload(_),
        ) => true,
        _ => false,
    }
}

fn is_proxy_transport_error(err: &FastbootError) -> bool {
    match err {
        FastbootError::Fastboot(e) => is_fastboot_transport_error(e),
        FastbootError::FlashError(FlashError::TimeoutError(_)) => true,
        _ => false,
    }
}

async fn handle_command<T: AsyncRead + AsyncWrite + Unpin>(
    ctx: &FastbootContext,
    cmd: &Command,
    interface: &mut T,
    timeout: Duration,
) -> Result<(), FastbootError> {
    match send_with_timeout(ctx.clone(), cmd.clone(), interface, timeout).await? {
        Reply::Okay(_) => Ok(()),
        Reply::Fail(message) => {
            Err(FlashError::StreamFailed { command: cmd.clone(), message }.into())
        }
        r => Err(FastbootError::UnexpectedReply { method: cmd.to_string(), reply: r.to_string() }),
    }
}

async fn report_upload_err_if_any(
    err: &FastbootError,
    progress_listener: &ProgressListener<'_>,
) -> Result<(), FastbootError> {
    if let FastbootError::Fastboot(fastboot::FastbootError::Upload(upload_err)) = err {
        log::error!("{upload_err}");
        progress_listener.on_error(upload_err).await.map_err(fastboot::FastbootError::from)?;
    }
    Ok(())
}

async fn upload_data<T: AsyncRead + AsyncWrite + Unpin>(
    ctx: &FastbootContext,
    data: &[u8],
    interface: &mut T,
    progress_listener: &ProgressListener<'_>,
    reported_bytes: &mut u64,
    timeout: Duration,
) -> Result<(), FastbootError> {
    let expected = data.len().try_into().unwrap();
    let reply =
        send_with_timeout(ctx.clone(), Command::Download(expected), interface, timeout).await?;
    let Reply::Data(received) = reply else {
        return Err(fastboot::FastbootError::from(UnexpectedReply { reply }).into());
    };
    if received != expected {
        return Err(fastboot::FastbootError::from(WrongSizeResponse { received, expected }).into());
    }

    log::trace!("fastboot: starting upload of {} bytes", data.len());
    let mut attempt_written = 0u64;
    // Don't just write_all all the data so that we get incremental progress updates.
    for chunk in data.chunks(BUFFER_SIZE) {
        log::trace!("fastboot: uploading {} bytes", chunk.len());
        if let Err(e) = interface.write_all(chunk).await {
            return Err(fastboot::FastbootError::from(CouldNotWriteToInterface(e)).into());
        }

        attempt_written += u64::try_from(chunk.len()).unwrap();
        if attempt_written > *reported_bytes {
            progress_listener
                .on_progress(attempt_written - *reported_bytes)
                .await
                .map_err(fastboot::FastbootError::from)?;
            *reported_bytes = attempt_written;
        }
    }

    match read_and_log_info_with_timeout(interface, timeout).await {
        Ok(Reply::Okay(_)) => {
            log::trace!("fastboot: uploaded {} bytes", expected);
            Ok(())
        }
        Ok(reply) => Err(fastboot::FastbootError::from(UnexpectedReply { reply }).into()),
        Err(err) => Err(fastboot::FastbootError::from(CouldNotVerifyUpload(err)).into()),
    }
}

impl<T, F> Fastboot for FastbootProxy<T, F>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
    F: InterfaceFactory<T>,
{
    fn supports_gbl_async_tasks(&self) -> bool {
        self.interface_factory.supports_gbl_async_tasks()
    }

    fn get_var<'a>(&'a mut self, name: &'a str) -> BoxFuture<'a, Result<String, FastbootError>> {
        Box::pin(async move {
            let command = Command::GetVar(ClientVariable::Oem(name.to_string()));
            let res = match send(self.ctx.clone(), command.clone(), self.interface().await?).await {
                Ok(r) => match r {
                    Reply::Okay(v) => {
                        log::trace!("Got var {}. Content: '{:?}", name, v);
                        Ok(v.trim_end_matches('\0').to_string())
                    }
                    Reply::Fail(message) => {
                        Err(FastbootError::GetVariableError { variable: name.to_string(), message })
                    }
                    r @ _ => Err(FastbootError::UnexpectedReply {
                        method: command.to_string(),
                        reply: r.to_string(),
                    }),
                },
                Err(e) => Err(e.into()),
            };
            self.invalidate_on_transport_error(res)
        })
    }

    fn get_all_vars(
        &mut self,
        listener: Sender<Variable>,
    ) -> BoxFuture<'_, Result<(), FastbootError>> {
        Box::pin(async move {
            let variable_listener = VariableListener::new(listener);
            let command = Command::GetVar(ClientVariable::All);
            let res = send_with_listener(
                self.ctx.clone(),
                command.clone(),
                self.interface().await?,
                &variable_listener,
            )
            .await
            .map_err(FastbootError::from);
            match self.invalidate_on_transport_error(res)? {
                Reply::Okay(_) => Ok(()),
                Reply::Fail(s) => Err(FastbootError::GetAllVarsFailed(s)),
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: command.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn flash<'a>(
        &'a mut self,
        partition_name: &'a str,
        path: &'a str,
        listener: Sender<UploadProgress>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let file_to_flash = File::open(path).map_err(FlashError::from)?;
            let size = file_to_flash.metadata().map_err(FlashError::from)?.len();
            let size = u32::try_from(size).map_err(|e| FlashError::InvalidFileSize(e))?;
            let progress_listener = ProgressListener::new(&listener);
            let upload_res = upload_with_read_timeout(
                self.ctx.clone(),
                size,
                file_to_flash,
                self.interface().await?,
                &progress_listener,
                timeout,
            )
            .await
            .map_err(FastbootError::from);
            let upload_reply = self.invalidate_on_transport_error(upload_res)?;
            match upload_reply {
                Reply::Okay(s) => log::debug!("Received response from download command: {}", s),
                Reply::Fail(s) => {
                    return Err(FastbootError::StageError(StageError::UploadFailed {
                        path: partition_name.to_string(),
                        message: s,
                    }));
                }
                r @ _ => {
                    return Err(FastbootError::UnexpectedReply {
                        method: Command::Download(size).to_string(),
                        reply: r.to_string(),
                    });
                }
            };
            self.flash_uploaded(partition_name, timeout).await
        })
    }

    fn flash_from_reader<'a>(
        &'a mut self,
        partition_name: &'a str,
        size: u32,
        reader: &'a mut (dyn std::io::Read + Send),
        listener: Sender<UploadProgress>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let progress_listener = ProgressListener::new(&listener);
            let upload_res = upload_from_reader(
                self.ctx.clone(),
                size,
                reader,
                self.interface().await?,
                &progress_listener,
                timeout,
            )
            .await
            .map_err(FastbootError::from);
            let upload_reply = self.invalidate_on_transport_error(upload_res)?;
            match upload_reply {
                Reply::Okay(s) => log::debug!("Received response from download command: {}", s),
                Reply::Fail(s) => {
                    return Err(FastbootError::StageError(StageError::UploadFailed {
                        path: partition_name.to_string(),
                        message: s,
                    }));
                }
                r @ _ => {
                    return Err(FastbootError::UnexpectedReply {
                        method: Command::Download(size).to_string(),
                        reply: r.to_string(),
                    });
                }
            };
            self.flash_uploaded(partition_name, timeout).await
        })
    }

    fn erase<'a>(
        &'a mut self,
        partition_name: &'a str,
    ) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let command = Command::Erase(partition_name.to_string());
            let res = send(self.ctx.clone(), command.clone(), self.interface().await?)
                .await
                .map_err(FastbootError::from);
            let reply = self.invalidate_on_transport_error(res)?;
            match reply {
                Reply::Okay(_) => {
                    log::debug!("Successfully erased parition: {}", partition_name);
                    Ok(())
                }
                Reply::Fail(s) => {
                    return Err(FastbootError::ErasePartitionFailed {
                        partition: partition_name.to_string(),
                        message: s,
                    });
                }
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: command.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn boot(&mut self) -> BoxFuture<'_, Result<(), FastbootError>> {
        Box::pin(async move {
            // Note: the target may not successfully send a response when asked to boot,
            // so let's use a short time-out, and treat a timeout error as a success.
            let reply = handle_timeout_as_okay(
                send_with_timeout(
                    self.ctx.clone(),
                    Command::Boot,
                    self.interface().await?,
                    Duration::seconds(3),
                )
                .await,
            )?;
            match reply {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent boot");
                    Ok(())
                }

                Reply::Fail(s) => return Err(FastbootError::BootFailed(s)),
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: Command::Reboot.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn reboot(&mut self) -> BoxFuture<'_, Result<(), FastbootError>> {
        Box::pin(async move {
            // Note: the target may not successfully send a response when asked to reboot,
            // so let's use a short time-out, and treat a timeout error as a success.
            let reply = handle_timeout_as_okay(
                send_with_timeout(
                    self.ctx.clone(),
                    Command::Reboot,
                    self.interface().await?,
                    Duration::seconds(3),
                )
                .await,
            )?;
            match reply {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent reboot");
                    Ok(())
                }
                Reply::Fail(s) => return Err(FastbootError::RebootFailed(s)),
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: Command::Reboot.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn reboot_bootloader(
        &mut self,
        listener: Sender<RebootEvent>,
    ) -> BoxFuture<'_, Result<(), FastbootError>> {
        Box::pin(async move {
            // Note: the target may not successfully send a response when asked to reboot-bootloader,
            // so let's use a short time-out, and treat a timeout error as a success.
            let reply = handle_timeout_as_okay(
                send_with_timeout(
                    self.ctx.clone(),
                    Command::RebootBootLoader,
                    self.interface().await?,
                    Duration::seconds(3),
                )
                .await,
            )?;
            match reply {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent reboot bootloader");
                    let send_res = listener.send(RebootEvent::OnReboot).await;
                    if send_res.is_err() {
                        log::debug!(
                            "reboot_bootloader hit error sending the reboot event to caller: {:#?}",
                            send_res
                        );
                    }
                }
                Reply::Fail(s) => return Err(FastbootError::RebootBootloaderFailed { message: s }),
                r @ _ => {
                    return Err(FastbootError::UnexpectedReply {
                        method: Command::RebootBootLoader.to_string(),
                        reply: r.to_string(),
                    });
                }
            };
            // Once the target is rebooted, reconnect
            self.reconnect().await?;
            Ok(())
        })
    }

    fn continue_boot(&mut self) -> BoxFuture<'_, Result<(), FastbootError>> {
        Box::pin(async move {
            // Note: the target may not successfully send a response when asked to continue,
            // so let's use a short time-out, and treat a timeout error as a success.
            let reply = handle_timeout_as_okay(
                send_with_timeout(
                    self.ctx.clone(),
                    Command::Continue,
                    self.interface().await?,
                    Duration::seconds(3),
                )
                .await,
            )?;
            match reply {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent continue");
                    Ok(())
                }
                Reply::Fail(s) => return Err(FastbootError::ContinueBootFailed(s)),
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: Command::Continue.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn get_staged<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let res = download(self.ctx.clone(), &path.to_string(), self.interface().await?)
                .await
                .map_err(FastbootError::from);
            match self.invalidate_on_transport_error(res)? {
                Reply::Okay(_) => {
                    log::debug!("Successfully downloaded to \"{}\"", path);
                    Ok(())
                }
                Reply::Fail(message) => {
                    return Err(FastbootError::DownloadFailed { path: path.to_string(), message });
                }
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: Command::Upload.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn stage<'a>(
        &'a mut self,
        path: &'a str,
        listener: Sender<UploadProgress>,
    ) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let progress_listener = ProgressListener::new(&listener);
            let file_to_stage = File::open(path).map_err(StageError::from)?;
            let size = file_to_stage.metadata().map_err(StageError::from)?.len();
            let size = u32::try_from(size).map_err(|e| StageError::InvalidFileSize(e))?;
            log::debug!("uploading file size: {}", size);
            let res = upload(
                self.ctx.clone(),
                size,
                file_to_stage,
                self.interface().await?,
                &progress_listener,
            )
            .await
            .map_err(FastbootError::from);
            match self.invalidate_on_transport_error(res)? {
                Reply::Okay(s) => {
                    log::debug!("Received response from download command: {}", s);
                    Ok(())
                }
                Reply::Fail(s) => {
                    return Err(FastbootError::StageError(StageError::UploadFailed {
                        path: path.to_string(),
                        message: s,
                    }));
                }
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: Command::Download(size).to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn set_active<'a>(&'a mut self, slot: &'a str) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            // Note: the target may not successfully send a response when asked to set active,
            // so let's use a short time-out, and treat a timeout error as a success.
            // See b/405436515 for more information.
            let command = Command::SetActive(slot.to_string());
            let reply = handle_timeout_as_okay(
                send_with_timeout(
                    self.ctx.clone(),
                    command.clone(),
                    self.interface().await?,
                    Duration::seconds(3),
                )
                .await,
            )?;
            match reply {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent set_active");
                    Ok(())
                }
                Reply::Fail(message) => {
                    return Err(FastbootError::SetActiveFailed { slot: slot.to_string(), message });
                }
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: command.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn oem<'a>(&'a mut self, command: &'a str) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let command = Command::Oem(command.to_string());
            let res = send(self.ctx.clone(), command.clone(), self.interface().await?)
                .await
                .map_err(FastbootError::from);
            match self.invalidate_on_transport_error(res)? {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent oem command \"{}\"", command);
                    Ok(())
                }
                Reply::Fail(message) => {
                    return Err(FastbootError::OemCommandFailed {
                        command: command.to_string(),
                        message,
                    });
                }
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: command.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn oem_with_timeout<'a>(
        &'a mut self,
        command: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let command = Command::Oem(command.to_string());
            let timeout = timeout.max(Duration::zero());
            let res = send_with_timeout(
                self.ctx.clone(),
                command.clone(),
                self.interface().await?,
                timeout,
            )
            .await
            .map_err(FastbootError::from);
            match self.invalidate_on_transport_error(res)? {
                Reply::Okay(_) => {
                    log::debug!("Successfully sent oem command \"{}\"", command);
                    Ok(())
                }
                Reply::Fail(message) => {
                    Err(FastbootError::OemCommandFailed { command: command.to_string(), message })
                }
                r @ _ => Err(FastbootError::UnexpectedReply {
                    method: command.to_string(),
                    reply: r.to_string(),
                }),
            }
        })
    }

    fn stream<'a>(
        &'a mut self,
        name: &'a str,
        stream_command: StreamCommand,
        listener: &'a Sender<UploadProgress>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<(), FastbootError>> {
        Box::pin(async move {
            let progress_listener = ProgressListener::new(listener);
            let StreamCommand { offset_bytes, op } = stream_command;
            let ctx = self.ctx.clone();

            let length_bytes = match op {
                StreamOp::Fill { val, length_bytes } => {
                    log::trace!("fastboot: filling {} bytes", length_bytes);
                    let finishing_cmd = Command::StreamFill {
                        partition: name.to_owned(),
                        offset_bytes,
                        length_bytes,
                        val,
                    };
                    for attempt in 0..2 {
                        let interface = self.interface().await?;
                        match handle_command(&ctx, &finishing_cmd, interface, timeout).await {
                            Ok(()) => break,
                            Err(err) if attempt == 0 && is_proxy_transport_error(&err) => {
                                log::warn!(
                                    "Transient transport error during stream-fill ({err}); \
                                     reconnecting and retrying once"
                                );
                                self.reconnect().await?;
                            }
                            Err(err) => return self.invalidate_on_transport_error(Err(err)),
                        }
                    }
                    progress_listener
                        .on_progress(length_bytes)
                        .await
                        .map_err(fastboot::FastbootError::from)?;
                    length_bytes
                }
                StreamOp::Flash { data, crc32 } => {
                    let _lock = ctx.lock_transfer().await;
                    let finishing_cmd =
                        Command::StreamFlash { partition: name.to_owned(), offset_bytes, crc32 };
                    let mut reported_bytes = 0u64;
                    for attempt in 0..2 {
                        let interface = self.interface().await?;
                        let attempt_res = async {
                            upload_data(
                                &ctx,
                                &data,
                                interface,
                                &progress_listener,
                                &mut reported_bytes,
                                timeout,
                            )
                            .await?;
                            handle_command(&ctx, &finishing_cmd, interface, timeout).await
                        }
                        .await;
                        match attempt_res {
                            Ok(()) => break,
                            Err(err) if attempt == 0 && is_proxy_transport_error(&err) => {
                                log::warn!(
                                    "Transient transport error during stream-flash ({err}); \
                                     reconnecting and retrying once"
                                );
                                if let Err(reconn_err) = self.reconnect().await {
                                    report_upload_err_if_any(&err, &progress_listener).await?;
                                    return Err(reconn_err.into());
                                }
                            }
                            Err(err) => {
                                if is_proxy_transport_error(&err) {
                                    log::warn!(
                                        "Invalidating cached fastboot interface after transport error: {err}"
                                    );
                                    self.interface = None;
                                }
                                report_upload_err_if_any(&err, &progress_listener).await?;
                                return Err(err);
                            }
                        }
                    }
                    u64::try_from(data.len()).unwrap()
                }
            };

            log::trace!("fastboot: streamed {length_bytes} bytes to {name}");
            Ok(())
        })
    }
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use crate::interface_factory::{InterfaceFactoryBase, InterfaceFactoryError};
    use fastboot::test_transport::TestTransport;
    use pretty_assertions::assert_eq;
    use rand::RngExt as _;
    use rand::rngs::SmallRng;
    use std::io::{Read, Seek, SeekFrom, Write};
    use tempfile::{NamedTempFile, TempDir};
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::Receiver;

    #[derive(Default, Debug, Clone)]
    struct TestTransportFactory {}

    impl InterfaceFactoryBase<TestTransport> for TestTransportFactory {
        async fn open(&mut self) -> Result<TestTransport, InterfaceFactoryError> {
            Ok(TestTransport::new())
        }

        async fn close(&self) {}

        async fn rediscover(&mut self) -> Result<(), InterfaceFactoryError> {
            Ok(())
        }
    }

    impl InterfaceFactory<TestTransport> for TestTransportFactory {}

    ///////////////////////////////////////////////////////////////////////////
    //  get_var
    //

    #[fuchsia::test]
    async fn test_get_var() -> Result<()> {
        {
            let mut test_transport = TestTransport::new();
            test_transport.push(Reply::Okay("0.4\0\0".to_string()));
            let mut fastboot_client = FastbootProxy {
                interface: Some(test_transport),
                interface_factory: TestTransportFactory {},
                ctx: FastbootContext::new(),
            };

            assert_eq!(fastboot_client.get_var(&"version").await?, "0.4");
        }
        {
            let mut test_transport = TestTransport::new();
            test_transport.push(Reply::Fail("variable doesnt exist".to_string()));
            let mut fastboot_client = FastbootProxy {
                interface: Some(test_transport),
                interface_factory: TestTransportFactory {},
                ctx: FastbootContext::new(),
            };

            assert!(fastboot_client.get_var("version").await.is_err())
        }
        {
            let mut test_transport = TestTransport::new();
            test_transport.push(Reply::Data(1234));
            let mut fastboot_client = FastbootProxy {
                interface: Some(test_transport),
                interface_factory: TestTransportFactory {},
                ctx: FastbootContext::new(),
            };

            assert!(fastboot_client.get_var("version").await.is_err())
        }
        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    //  get_all_vars
    //

    #[fuchsia::test]
    async fn test_get_all_vars() -> Result<()> {
        let (var_client, mut var_server): (Sender<Variable>, Receiver<Variable>) = mpsc::channel(3);
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Info("sis:corona".to_string()),
            Reply::Info("cav:babs".to_string()),
            Reply::Info("name:ianthe".to_string()),
            Reply::Okay("Done".to_string()),
        ]);
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let _ = fastboot_client.get_all_vars(var_client).await?;
        assert_eq!(
            var_server.recv().await,
            Some(Variable { name: "sis".to_string(), value: "corona".to_string() })
        );

        assert_eq!(
            var_server.recv().await,
            Some(Variable { name: "cav".to_string(), value: "babs".to_string() })
        );
        assert_eq!(
            var_server.recv().await,
            Some(Variable { name: "name".to_string(), value: "ianthe".to_string() })
        );
        assert!(var_server.recv().await.is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_get_all_vars_error() -> Result<()> {
        let (var_client, mut var_server): (Sender<Variable>, Receiver<Variable>) = mpsc::channel(2);
        let mut test_transport = TestTransport::new();
        test_transport
            .extend([Reply::Info("alt:kiriona".to_string()), Reply::Fail("Done".to_string())]);
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };
        assert!(fastboot_client.get_all_vars(var_client).await.is_err());
        assert_eq!(
            var_server.recv().await,
            Some(Variable { name: "alt".to_string(), value: "kiriona".to_string() })
        );
        assert!(var_server.recv().await.is_none());
        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // oem
    //

    #[fuchsia::test]
    async fn test_oem_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Info("info line 1".to_string()));
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.oem("version").await.is_ok());
        Ok(())
    }
    #[fuchsia::test]
    async fn test_oem_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("this command failed".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.oem("version").await.is_err());

        Ok(())
    }
    #[fuchsia::test]
    async fn test_oem_bail_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.oem("version").await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // erase
    //

    #[fuchsia::test]
    async fn test_erase_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.erase("slotA").await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn test_erase_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not erase".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.erase("slotB").await.is_err());

        Ok(())
    }
    #[fuchsia::test]
    async fn test_erase_bail_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.erase("slotC").await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // boot
    //

    #[fuchsia::test]
    async fn test_boot_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.boot().await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn test_boot_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not boot".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.boot().await.is_err());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_boot_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.boot().await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // reboot
    //

    #[fuchsia::test]
    async fn test_reboot_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.reboot().await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn test_reboot_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not reboot".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.reboot().await.is_err());

        Ok(())
    }
    #[fuchsia::test]
    async fn test_reboot_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.reboot().await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // reboot_bootloader
    //

    #[fuchsia::test]
    async fn test_reboot_bootloader_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<RebootEvent>, Receiver<RebootEvent>) =
            mpsc::channel(3);

        fastboot_client.reboot_bootloader(var_client).await?;

        assert_eq!(var_server.recv().await, Some(RebootEvent::OnReboot));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_reboot_bootloader_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not reboot bootloader".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<RebootEvent>, Receiver<RebootEvent>) =
            mpsc::channel(3);
        assert!(fastboot_client.reboot_bootloader(var_client).await.is_err());

        assert!(var_server.recv().await.is_none());

        Ok(())
    }
    #[fuchsia::test]
    async fn test_reboot_bootloader_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<RebootEvent>, Receiver<RebootEvent>) =
            mpsc::channel(3);
        assert!(fastboot_client.reboot_bootloader(var_client).await.is_err());

        assert!(var_server.recv().await.is_none());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // continue_boot
    //

    #[fuchsia::test]
    async fn test_continue_boot_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.continue_boot().await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn test_continue_boot_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not continue boot".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.continue_boot().await.is_err());

        Ok(())
    }
    #[fuchsia::test]
    async fn test_continue_boot_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.continue_boot().await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // set_active
    //

    #[fuchsia::test]
    async fn test_set_active_ok() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Okay("done".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.set_active("slotA").await?;
        Ok(())
    }

    #[fuchsia::test]
    async fn test_set_active_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not set active".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.set_active("slotB").await.is_err());

        Ok(())
    }
    #[fuchsia::test]
    async fn test_set_active_bail_unexpected_response() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Data(1234));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.set_active("slotC").await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // get_staged
    //

    #[fuchsia::test]
    async fn test_get_staged_ok() -> Result<()> {
        let tmpdir = TempDir::new().unwrap();

        // Generate temporary file
        let (mut file, temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(12),                 // Upload Response (size)
            Reply::Data(1234),               // Upload Response
            Reply::Okay("Done".to_string()), // Upload Done
        ]);

        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.get_staged(temp_path.to_str().unwrap()).await?;

        let mut buf = Vec::<u8>::new();
        file.read_to_end(&mut buf)?;
        assert_eq!(buf, "DATA000004D2".as_bytes());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_get_staged_fail() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not get staged".to_string()));
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.get_staged("slotB").await.is_err());

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // stage
    //

    #[fuchsia::test]
    async fn test_stage_ok() -> Result<()> {
        let tmpdir = TempDir::new().unwrap();
        // Generate a large temporary file
        let (mut file, temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();
        let mut rng: SmallRng = rand::make_rng();
        let mut buf = Vec::<u8>::new();
        buf.resize(1 * 4096, 0);
        rng.fill(&mut buf[..]);
        file.write_all(&buf).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(4096),               // Download Response
            Reply::Okay("done".to_string()), // Download Okay
        ]);
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<UploadProgress>, Receiver<UploadProgress>) =
            mpsc::channel(3);

        fastboot_client.stage(temp_path.to_str().unwrap(), var_client).await?;

        assert!(matches!(var_server.recv().await, Some(UploadProgress::OnStarted { size: 4096 })));
        assert!(matches!(
            var_server.recv().await,
            Some(UploadProgress::OnProgress { bytes_written: 4096 })
        ));
        assert!(matches!(var_server.recv().await, Some(UploadProgress::OnFinished)));
        assert!(var_server.recv().await.is_none());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_stage_fail() -> Result<()> {
        let tmpdir = TempDir::new().unwrap();

        // Generate a large temporary file
        let (mut file, temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();
        let mut rng: SmallRng = rand::make_rng();
        let mut buf = Vec::<u8>::new();
        buf.resize(1 * 4096, 0);
        rng.fill(&mut buf[..]);
        file.write_all(&buf).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not stage".to_string()));

        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<UploadProgress>, Receiver<UploadProgress>) =
            mpsc::channel(2);

        assert!(fastboot_client.stage(temp_path.to_str().unwrap(), var_client).await.is_err());

        assert!(var_server.recv().await.is_none());
        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // flash
    //

    #[fuchsia::test]
    async fn test_flash_ok() -> Result<()> {
        let tmpdir = TempDir::new().unwrap();

        // Generate a large temporary file
        let (mut file, temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();
        let mut rng: SmallRng = rand::make_rng();
        let mut buf = Vec::<u8>::new();
        buf.resize(1 * 4096, 0);
        rng.fill(&mut buf[..]);
        file.write_all(&buf).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(4096),           // Download
            Reply::Okay("".to_string()), // Download Ok
            Reply::Okay("".to_string()), // Flash Ok
        ]);

        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<UploadProgress>, Receiver<UploadProgress>) =
            mpsc::channel(3);

        fastboot_client
            .flash("partition1", temp_path.to_str().unwrap(), var_client, Duration::seconds(1))
            .await?;

        assert!(matches!(var_server.recv().await, Some(UploadProgress::OnStarted { size: 4096 })));
        assert!(matches!(
            var_server.recv().await,
            Some(UploadProgress::OnProgress { bytes_written: 4096 })
        ));
        assert!(matches!(var_server.recv().await, Some(UploadProgress::OnFinished)));
        assert!(var_server.recv().await.is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_flash_fail() -> Result<()> {
        let tmpdir = TempDir::new().unwrap();

        // Generate a large temporary file
        let (mut file, temp_path) = NamedTempFile::new_in(&tmpdir).unwrap().into_parts();
        let mut rng: SmallRng = rand::make_rng();
        let mut buf = Vec::<u8>::new();
        buf.resize(1 * 4096, 0);
        rng.fill(&mut buf[..]);
        file.write_all(&buf).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        let mut test_transport = TestTransport::new();
        test_transport.push(Reply::Fail("could not stage".to_string()));

        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (var_client, mut var_server): (Sender<UploadProgress>, Receiver<UploadProgress>) =
            mpsc::channel(2);

        assert!(
            fastboot_client
                .flash("partition1", temp_path.to_str().unwrap(), var_client, Duration::seconds(1))
                .await
                .is_err()
        );

        assert!(var_server.recv().await.is_none());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_stream_non_empty_okay_replies() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(4),
            Reply::Okay("0.005s".to_string()), // Non-empty download ack
            Reply::Okay("flashed ok".to_string()), // Non-empty stream-flash ack
            Reply::Okay("filled ok".to_string()), // Non-empty stream-fill ack
        ]);

        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        let (progress_tx, mut progress_rx) = mpsc::channel(4);

        let flash_cmd = StreamCommand {
            offset_bytes: 0,
            op: StreamOp::Flash { data: bytes::Bytes::from_static(&[1, 2, 3, 4]), crc32: 0x1234 },
        };
        fastboot_client.stream("zircon_a", flash_cmd, &progress_tx, Duration::seconds(1)).await?;

        let fill_cmd =
            StreamCommand { offset_bytes: 4, op: StreamOp::Fill { val: 0, length_bytes: 4096 } };
        fastboot_client.stream("zircon_a", fill_cmd, &progress_tx, Duration::seconds(1)).await?;

        assert_eq!(progress_rx.recv().await, Some(UploadProgress::OnProgress { bytes_written: 4 }));
        assert_eq!(
            progress_rx.recv().await,
            Some(UploadProgress::OnProgress { bytes_written: 4096 })
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_stream_upload_acquires_transfer_lock() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport.extend([
            Reply::Data(4),
            Reply::Okay("".to_string()),
            Reply::Okay("".to_string()),
        ]);

        let ctx = FastbootContext::new();
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: ctx.clone(),
        };

        let guard = ctx.lock_transfer().await;
        let (progress_tx, _progress_rx) = mpsc::channel(2);
        let flash_cmd = StreamCommand {
            offset_bytes: 0,
            op: StreamOp::Flash { data: bytes::Bytes::from_static(&[1, 2, 3, 4]), crc32: 0x1234 },
        };

        let mut stream_fut = Box::pin(fastboot_client.stream(
            "zircon_a",
            flash_cmd,
            &progress_tx,
            Duration::seconds(1),
        ));
        assert!(futures::poll!(&mut stream_fut).is_pending());

        drop(guard);
        stream_fut.await?;
        Ok(())
    }

    #[derive(Debug)]
    struct QueueTransportFactory {
        transports: std::collections::VecDeque<TestTransport>,
        rediscover_count: usize,
        open_count: usize,
    }

    impl QueueTransportFactory {
        fn new(transports: impl IntoIterator<Item = TestTransport>) -> Self {
            Self {
                transports: transports.into_iter().collect(),
                rediscover_count: 0,
                open_count: 0,
            }
        }
    }

    impl InterfaceFactoryBase<TestTransport> for QueueTransportFactory {
        async fn open(&mut self) -> Result<TestTransport, InterfaceFactoryError> {
            self.open_count += 1;
            Ok(self.transports.pop_front().unwrap_or_else(TestTransport::new))
        }

        async fn close(&self) {}

        async fn rediscover(&mut self) -> Result<(), InterfaceFactoryError> {
            self.rediscover_count += 1;
            Ok(())
        }
    }

    impl InterfaceFactory<TestTransport> for QueueTransportFactory {}

    #[fuchsia::test]
    async fn test_stream_reconnects_and_retries_once_on_transport_error() -> Result<()> {
        // First transport accepts `Download` and payload bytes (reporting progress), then drops
        // EOF while waiting for the download ACK.
        let mut broken = TestTransport::new();
        broken.push(Reply::Data(8));

        // Second transport completes the retried `StreamFlash` and a subsequent `StreamFill`
        // that also recovers after one EOF.
        let mut recovered_flash = TestTransport::new();
        recovered_flash.extend([
            Reply::Data(8),
            Reply::Okay("".to_string()),
            Reply::Okay("".to_string()),
            // Next command (`StreamFill`) hits EOF on this connection...
        ]);
        let mut recovered_fill = TestTransport::new();
        recovered_fill.push(Reply::Okay("".to_string()));

        let mut fastboot_client = FastbootProxy {
            interface: Some(broken),
            interface_factory: QueueTransportFactory::new([recovered_flash, recovered_fill]),
            ctx: FastbootContext::new(),
        };

        let (progress_tx, mut progress_rx) = mpsc::channel(8);
        let flash_cmd = StreamCommand {
            offset_bytes: 0,
            op: StreamOp::Flash {
                data: bytes::Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]),
                crc32: 0x9999,
            },
        };
        fastboot_client.stream("fvm", flash_cmd, &progress_tx, Duration::seconds(1)).await?;

        let fill_cmd =
            StreamCommand { offset_bytes: 8, op: StreamOp::Fill { val: 0, length_bytes: 4096 } };
        fastboot_client.stream("fvm", fill_cmd, &progress_tx, Duration::seconds(1)).await?;
        drop(progress_tx);

        let mut events = vec![];
        while let Some(ev) = progress_rx.recv().await {
            events.push(ev);
        }
        assert_eq!(
            events,
            vec![
                UploadProgress::OnProgress { bytes_written: 8 },
                UploadProgress::OnProgress { bytes_written: 4096 },
            ],
            "retried stream command must not duplicate progress or emit spurious OnError"
        );
        assert_eq!(fastboot_client.interface_factory.rediscover_count, 2);
        assert_eq!(fastboot_client.interface_factory.open_count, 2);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_transport_error_invalidates_cached_interface() -> Result<()> {
        // Empty transport yields EOF (`ReplyTooShort { reply_len: 0 }`).
        let broken = TestTransport::new();
        let mut fresh = TestTransport::new();
        fresh.push(Reply::Okay("0.4".to_string()));

        let mut fastboot_client = FastbootProxy {
            interface: Some(broken),
            interface_factory: QueueTransportFactory::new([fresh]),
            ctx: FastbootContext::new(),
        };

        assert!(fastboot_client.get_var("version").await.is_err());
        assert!(
            fastboot_client.interface.is_none(),
            "transport EOF must invalidate cached interface"
        );
        assert_eq!(fastboot_client.get_var("version").await?, "0.4");
        assert_eq!(fastboot_client.interface_factory.open_count, 1);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_oem_with_timeout() -> Result<()> {
        let mut test_transport = TestTransport::new();
        test_transport
            .extend([Reply::Okay("".to_string()), Reply::Fail("sync failed".to_string())]);
        let mut fastboot_client = FastbootProxy {
            interface: Some(test_transport),
            interface_factory: TestTransportFactory {},
            ctx: FastbootContext::new(),
        };

        fastboot_client.oem_with_timeout("gbl-sync-tasks", Duration::seconds(120)).await?;
        assert!(matches!(
            fastboot_client.oem_with_timeout("gbl-sync-tasks", Duration::seconds(120)).await,
            Err(FastbootError::OemCommandFailed { .. })
        ));
        Ok(())
    }
}
