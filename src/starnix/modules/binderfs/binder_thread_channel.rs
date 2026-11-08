// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reads, decodes and replies to the messages of `fuchsia.starnix.binder/BinderThread` channels,
//! without the FIDL server bindings.

// TODO(https://fxbug.dev/581868303): This module is an interim stage, until the Rust FIDL bindings
// can serve a protocol without an async executor. The worker tasks that serve `BinderThread`
// channels wait for requests with an `ObjectWaiter`, so that signals, exit and freezing interrupt
// them, and so that a request wakes them up without a port. The FIDL server bindings need an
// executor, so this module decodes and encodes the messages with the `fidl::encoding` internals
// that the generated bindings use, and handles unknown methods as the bindings do. Replace it with
// the support of the FIDL bindings (possibly the `fidl_next` bindings) once it is available.

use fidl::encoding::{
    Decode, Decoder, DefaultFuchsiaResourceDialect, DynamicFlags, EmptyStruct, Encoder, Flexible,
    FlexibleResult, FlexibleResultType, FlexibleType, FrameworkErr, TransactionHeader,
    TransactionMessage, TransactionMessageType, decode_transaction_header, with_tls_decode_buf,
    with_tls_encoded,
};
use fidl_fuchsia_posix as fposix;
use fidl_fuchsia_starnix_binder as fbinder;
use starnix_logging::log_warn;

/// A message read on a `BinderThread` channel.
pub enum ThreadChannelMessage {
    /// An ioctl, to reply to with `send_ioctl_reply` and the transaction ID `tx_id`.
    Ioctl { tx_id: u32, request: fbinder::BinderThreadIoctlRequest },
    /// A message of an unknown flexible method, that was already handled.
    Unknown,
}

/// The result of `read`.
pub enum ReadResult {
    /// A message was read.
    Message(ThreadChannelMessage),
    /// There is no message to read yet.
    ShouldWait,
    /// The channel must be closed: the client closed it, a message could not be read or decoded, or
    /// the reply to an unknown method could not be sent.
    Closed,
}

/// Reads and decodes the next message on `channel`, without waiting, as the FIDL bindings do.
///
/// Replies to the messages of unknown flexible two-way methods.
pub fn read(channel: &zx::Channel) -> ReadResult {
    with_tls_decode_buf::<_, DefaultFuchsiaResourceDialect>(|bytes, handles| {
        match channel.read_etc_split(bytes, handles) {
            Ok(()) => match decode(channel, bytes, handles) {
                Some(message) => ReadResult::Message(message),
                None => ReadResult::Closed,
            },
            Err(zx::Status::SHOULD_WAIT) => ReadResult::ShouldWait,
            Err(_) => ReadResult::Closed,
        }
    })
}

/// Decodes a message read on `channel`, in `bytes` and `handles`.
///
/// Replies to the messages of unknown flexible two-way methods. Returns `None` if the channel must
/// be closed.
fn decode(
    channel: &zx::Channel,
    bytes: &mut Vec<u8>,
    handles: &mut Vec<zx::HandleInfo>,
) -> Option<ThreadChannelMessage> {
    let (header, body) = match decode_transaction_header(bytes) {
        Ok(header_and_body) => header_and_body,
        Err(e) => {
            log_warn!("Invalid BinderThread message: {e:?}");
            return None;
        }
    };
    if header.ordinal == fbinder::binder_thread_ordinals::IOCTL {
        let mut request = fbinder::BinderThreadIoctlRequest::new_empty();
        let decoded =
            header.validate_request_tx_id(fidl::MethodType::TwoWay).and_then(|()| {
                Decoder::<DefaultFuchsiaResourceDialect>::decode_into::<
                    fbinder::BinderThreadIoctlRequest,
                >(&header, body, handles, &mut request)
            });
        return match decoded {
            Ok(()) => Some(ThreadChannelMessage::Ioctl { tx_id: header.tx_id, request }),
            Err(e) => {
                log_warn!("Invalid BinderThread.Ioctl request: {e:?}");
                None
            }
        };
    }
    if !header.dynamic_flags().contains(DynamicFlags::FLEXIBLE) {
        log_warn!("Unknown strict BinderThread ordinal: {}", header.ordinal);
        return None;
    }
    // As for unknown `Binder` methods, keep serving the channel.
    log_warn!("Unknown BinderThread ordinal: {}", header.ordinal);
    if header.tx_id != 0 {
        // Reply to an unknown two-way method with an `UnknownMethod` framework error, as the
        // bindings do. RFC-0138 requires closing the handles of the request before replying.
        // This runs in `with_tls_decode_buf`, so encode the reply in `bytes`: `with_tls_encoded`
        // would borrow the same thread-local buffers again, and panic.
        handles.clear();
        let reply = TransactionMessage {
            header: TransactionHeader::new(header.tx_id, header.ordinal, header.dynamic_flags()),
            body: Flexible::<()>::FrameworkErr(FrameworkErr::UnknownMethod),
        };
        let mut handle_dispositions = Vec::new();
        let sent = Encoder::<DefaultFuchsiaResourceDialect>::encode::<
            TransactionMessageType<FlexibleType<EmptyStruct>>,
        >(bytes, &mut handle_dispositions, reply)
        .is_ok()
            && channel.write_etc(bytes, &mut []).is_ok();
        if !sent {
            return None;
        }
    }
    Some(ThreadChannelMessage::Unknown)
}

/// Replies on `channel` to the `Ioctl` request with the transaction ID `tx_id`.
pub fn send_ioctl_reply(
    channel: &zx::Channel,
    tx_id: u32,
    result: Result<&[fbinder::IoctlReadWrite], fposix::Errno>,
) -> Result<(), fidl::Error> {
    let reply = TransactionMessage {
        header: TransactionHeader::new(
            tx_id,
            fbinder::binder_thread_ordinals::IOCTL,
            DynamicFlags::FLEXIBLE,
        ),
        body: FlexibleResult::new(result.map(|writes| (writes,))),
    };
    with_tls_encoded::<
        TransactionMessageType<
            FlexibleResultType<fbinder::BinderThreadIoctlResponse, fposix::Errno>,
        >,
        DefaultFuchsiaResourceDialect,
        (),
    >(reply, |bytes, handles| {
        channel.write_etc(bytes, handles).map_err(|e| fidl::Error::ServerResponseWrite(e.into()))
    })
}
