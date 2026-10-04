// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;
use std::task::{Wake, Waker};
use zx_types;

struct HandleReadWaker {
    handle: zx_types::zx_handle_t,
    notification_sender: Option<async_channel::Sender<zx_types::zx_handle_t>>,
}

impl Wake for HandleReadWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(ref sender) = self.notification_sender {
            let _ = sender
                .try_send(self.handle)
                .map_err(|e| log::debug!("failed sending notification {e:?}"));
        }
    }
}

pub(crate) fn handle_notifier_waker(
    handle: zx_types::zx_handle_t,
    notification_sender: Option<async_channel::Sender<zx_types::zx_handle_t>>,
) -> Waker {
    Waker::from(Arc::new(HandleReadWaker { handle, notification_sender }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::TryRecvError;

    #[test]
    fn clone_and_wake_send_once() {
        let (sender, receiver) = async_channel::bounded(2);
        let waker = handle_notifier_waker(7, Some(sender));
        let cloned = waker.clone();
        assert_eq!(receiver.sender_count(), 1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

        waker.wake_by_ref();
        assert_eq!(receiver.try_recv(), Ok(7));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

        cloned.wake();
        assert_eq!(receiver.try_recv(), Ok(7));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

        waker.wake();
        assert_eq!(receiver.try_recv(), Ok(7));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn last_waker_drop_releases_sender() {
        let (sender, receiver) = async_channel::unbounded();
        let waker = handle_notifier_waker(7, Some(sender));
        let cloned = waker.clone();
        drop(waker);
        assert_eq!(receiver.sender_count(), 1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        drop(cloned);
        assert_eq!(receiver.sender_count(), 0);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn wake_without_sender() {
        let waker = handle_notifier_waker(7, None);
        let cloned = waker.clone();
        waker.wake_by_ref();
        cloned.wake();
        waker.wake();
    }

    #[test]
    fn full_channel_keeps_queued_notification() {
        let (sender, receiver) = async_channel::bounded(1);
        sender.try_send(42).unwrap();
        let waker = handle_notifier_waker(7, Some(sender));
        waker.wake_by_ref();
        assert_eq!(receiver.try_recv(), Ok(42));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

        waker.wake_by_ref();
        waker.wake();
        assert_eq!(receiver.try_recv(), Ok(7));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn closed_channel_ignores_notification() {
        let (sender, receiver) = async_channel::unbounded();
        receiver.close();
        let waker = handle_notifier_waker(7, Some(sender));
        waker.wake_by_ref();
        waker.wake();
        assert_eq!(receiver.sender_count(), 0);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn wake_from_other_threads() {
        let (sender, receiver) = async_channel::unbounded();
        let waker = handle_notifier_waker(7, Some(sender));
        let cloned = waker.clone();
        std::thread::scope(|scope| {
            scope.spawn(|| waker.wake_by_ref());
            scope.spawn(move || cloned.wake());
        });
        assert_eq!(receiver.try_recv(), Ok(7));
        assert_eq!(receiver.try_recv(), Ok(7));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        drop(waker);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }
}
