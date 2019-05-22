use futures_core::future::{Future, FusedFuture};
use futures_core::task::{Context, Poll, Waker};
use core::pin::Pin;
use crate::intrusive_singly_linked_list::{ListNode};
use super::ChannelSendError;

/// Tracks how the future had interacted with the channel
#[derive(PartialEq, Debug)]
pub enum RecvPollState {
    /// The task is not registered at the wait queue at the channel
    Unregistered,
    /// The task was added to the wait queue at the channel.
    Registered,
    /// The task was notified that a value is available or can be sent,
    /// but hasn't interacted with the channel since then
    Notified,
}

/// Tracks the channel futures waiting state.
/// Access to this struct is synchronized through the channel.
#[derive(Debug)]
pub struct RecvWaitQueueEntry {
    /// The task handle of the waiting task
    pub task: Option<Waker>,
    /// Current polling state
    pub state: RecvPollState,
}

impl RecvWaitQueueEntry {
    /// Creates a new RecvWaitQueueEntry
    pub fn new() -> RecvWaitQueueEntry {
        RecvWaitQueueEntry {
            task: None,
            state: RecvPollState::Unregistered,
        }
    }
}

/// Tracks how the future had interacted with the channel
#[derive(PartialEq, Debug)]
pub enum SendPollState {
    /// The task is not registered at the wait queue at the channel
    Unregistered,
    /// The task was added to the wait queue at the channel.
    Registered,
    /// The value has been transmitted to the other task
    SendComplete,
}

/// Tracks the channel futures waiting state.
/// Access to this struct is synchronized through the channel.
pub struct SendWaitQueueEntry<T> {
    /// The task handle of the waiting task
    pub task: Option<Waker>,
    /// Current polling state
    pub state: SendPollState,
    /// The value to send
    pub value: Option<T>,
}

impl<T> core::fmt::Debug for SendWaitQueueEntry<T> {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> core::result::Result<(), core::fmt::Error> {
        fmt.debug_struct("SendWaitQueueEntry")
            .field("task", &self.task)
            .field("state", &self.state)
            .finish()
    }
}

impl<T> SendWaitQueueEntry<T> {
    /// Creates a new SendWaitQueueEntry
    pub fn new(value: T) -> SendWaitQueueEntry<T> {
        SendWaitQueueEntry {
            task: None,
            state: SendPollState::Unregistered,
            value: Some(value),
        }
    }
}

/// Adapter trait that allows Futures to generically interact with Channel
/// implementations via dynamic dispatch.
pub trait ChannelSendAccess<T> {
    unsafe fn try_send(
        &self,
        wait_node: &mut ListNode<SendWaitQueueEntry<T>>,
        cx: &mut Context<'_>,
    ) -> (Poll<()>, Option<T>);

    fn remove_send_waiter(&self, wait_node: &mut ListNode<SendWaitQueueEntry<T>>);
}

/// Adapter trait that allows Futures to generically interact with Channel
/// implementations via dynamic dispatch.
pub trait ChannelReceiveAccess<T> {
    unsafe fn try_receive(
        &self,
        wait_node: &mut ListNode<RecvWaitQueueEntry>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<T>>;

    fn remove_receive_waiter(&self, wait_node: &mut ListNode<RecvWaitQueueEntry>);
}

/// A Future that is returned by the `receive` function on a channel.
/// The future gets resolved with `Some(value)` when a value could be
/// received from the channel.
/// If the channels gets closed and no items are still enqueued inside the
/// channel, the future will resolve to `None`.
#[must_use = "futures do nothing unless polled"]
pub struct ChannelReceiveFuture<'a, T>
{
    /// The channel that is associated with this ChannelReceiveFuture
    pub(crate) channel: Option<&'a dyn ChannelReceiveAccess<T>>,
    /// Node for waiting on the channel
    pub(crate) wait_node: ListNode<RecvWaitQueueEntry>,
}

impl<'a, T> core::fmt::Debug for ChannelReceiveFuture<'a, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("ChannelReceiveFuture")
            .finish()
    }
}

impl<'a, T> Future for ChannelReceiveFuture<'a, T>
{
    type Output = Option<T>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<T>> {
        // It might be possible to use Pin::map_unchecked here instead of the two unsafe APIs.
        // However this didn't seem to work for some borrow checker reasons

        // Safety: The next operations are safe, because Pin promises us that
        // the address of the wait queue entry inside ChannelReceiveFuture is stable,
        // and we don't move any fields inside the future until it gets dropped.
        let mut_self: &mut ChannelReceiveFuture<T> = unsafe {
            Pin::get_unchecked_mut(self)
        };

        let channel = mut_self.channel.expect("polled ChannelReceiveFuture after completion");

        let poll_res = unsafe {
            channel.try_receive(&mut mut_self.wait_node, cx)
        };

        if poll_res.is_ready() {
            // A value was available
            mut_self.channel = None;
        }

        poll_res
    }
}

impl<'a, T> FusedFuture for ChannelReceiveFuture<'a, T> {
    fn is_terminated(&self) -> bool {
        self.channel.is_none()
    }
}

impl<'a, T> Drop for ChannelReceiveFuture<'a, T> {
    fn drop(&mut self) {
        // If this ChannelReceiveFuture has been polled and it was added to the
        // wait queue at the channel, it must be removed before dropping.
        // Otherwise the channel would access invalid memory.
        if let Some(channel) = self.channel {
            channel.remove_receive_waiter(&mut self.wait_node);
        }
    }
}

/// A Future that is returned by the `send` function on a channel.
/// The future gets resolved with `None` when a value could be
/// written to the channel.
/// If the channel gets closed the send operation will fail, and the
/// Future will resolve to `ChannelSendError(T)` and return the item to send.
#[must_use = "futures do nothing unless polled"]
pub struct ChannelSendFuture<'a, T>
{
    /// The Channel that is associated with this ChannelSendFuture
    pub(crate) channel: Option<&'a dyn ChannelSendAccess<T>>,
    /// Node for waiting on the channel
    pub(crate) wait_node: ListNode<SendWaitQueueEntry<T>>,
}

impl<'a, T> core::fmt::Debug for ChannelSendFuture<'a, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("ChannelSendFuture")
            .finish()
    }
}

impl<'a, T> ChannelSendFuture<'a, T> {
    /// Tries to cancel the ongoing send operation
    pub fn cancel(&mut self) -> Option<T> {
        let channel = self.channel.take();
        match channel {
            None => None,
            Some(channel) => {
                channel.remove_send_waiter(&mut self.wait_node);
                self.wait_node.value.take()
            }
        }
    }
}

impl<'a, T> Future for ChannelSendFuture<'a, T> {
    type Output = Result<(), ChannelSendError<T>>;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ChannelSendError<T>>> {
        // It might be possible to use Pin::map_unchecked here instead of the two unsafe APIs.
        // However this didn't seem to work for some borrow checker reasons

        // Safety: The next operations are safe, because Pin promises us that
        // the address of the wait queue entry inside ChannelSendFuture is stable,
        // and we don't move any fields inside the future until it gets dropped.
        let mut_self: &mut ChannelSendFuture<T> = unsafe {
            Pin::get_unchecked_mut(self)
        };

        let channel = mut_self.channel.expect("polled ChannelSendFuture after completion");

        let send_res = unsafe {
            channel.try_send(&mut mut_self.wait_node, cx)
        };

        match send_res.0 {
            Poll::Ready(()) => {
                // Value has been transmitted or channel was closed
                mut_self.channel = None;
                match send_res.1 {
                    Some(v) => {
                        // Channel must have been closed
                        Poll::Ready(Err(ChannelSendError(v)))
                    },
                    None => Poll::Ready(Ok(()))
                }
            },
            Poll::Pending => {
                Poll::Pending
            },
        }
    }
}

impl<'a, T> FusedFuture for ChannelSendFuture<'a, T> {
    fn is_terminated(&self) -> bool {
        self.channel.is_none()
    }
}

impl<'a, T> Drop for ChannelSendFuture<'a, T> {
    fn drop(&mut self) {
        // If this ChannelSendFuture has been polled and it was added to the
        // wait queue at the channel, it must be removed before dropping.
        // Otherwise the channel would access invalid memory.
        if let Some(channel) = self.channel {
            channel.remove_send_waiter(&mut self.wait_node);
        }
    }
}

// The next section should really integrated if the alloc feature is active,
// since it mainly requires `Arc` to be available. However for simplicity reasons
// it is currently only activated in std environments.
#[cfg(feature = "std")]
mod if_alloc {
    use super::*;

    pub mod shared {
        use super::*;

        /// A Future that is returned by the `receive` function on a channel.
        /// The future gets resolved with `Some(value)` when a value could be
        /// received from the channel.
        /// If the channels gets closed and no items are still enqueued inside the
        /// channel, the future will resolve to `None`.
        #[must_use = "futures do nothing unless polled"]
        pub struct ChannelReceiveFuture<T> {
            /// The Channel that is associated with this ChannelReceiveFuture
            pub(crate) channel: Option<std::sync::Arc<dyn ChannelReceiveAccess<T>>>,
            /// Node for waiting on the channel
            pub(crate) wait_node: ListNode<RecvWaitQueueEntry>,
        }

        impl<T> core::fmt::Debug for ChannelReceiveFuture<T> {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.debug_struct("ChannelReceiveFuture")
                    .finish()
            }
        }

        impl<T> Future for ChannelReceiveFuture<T> {
            type Output = Option<T>;

            fn poll(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<T>> {
                // It might be possible to use Pin::map_unchecked here instead of the two unsafe APIs.
                // However this didn't seem to work for some borrow checker reasons

                // Safety: The next operations are safe, because Pin promises us that
                // the address of the wait queue entry inside ChannelReceiveFuture is stable,
                // and we don't move any fields inside the future until it gets dropped.
                let mut_self: &mut ChannelReceiveFuture<T> = unsafe {
                    Pin::get_unchecked_mut(self)
                };

                let channel = mut_self.channel.take().expect(
                    "polled ChannelReceiveFuture after completion");

                let poll_res = unsafe {
                    channel.try_receive(&mut mut_self.wait_node, cx)
                };

                if poll_res.is_ready() {
                    // A value was available
                    mut_self.channel = None;
                }
                else {
                    mut_self.channel = Some(channel)
                }

                poll_res
            }
        }

        impl<T> FusedFuture for ChannelReceiveFuture<T> {
            fn is_terminated(&self) -> bool {
                self.channel.is_none()
            }
        }

        impl<T> Drop for ChannelReceiveFuture<T> {
            fn drop(&mut self) {
                // If this ChannelReceiveFuture has been polled and it was added to the
                // wait queue at the channel, it must be removed before dropping.
                // Otherwise the channel would access invalid memory.
                if let Some(channel) = &self.channel {
                    channel.remove_receive_waiter(&mut self.wait_node);
                }
            }
        }

        /// A Future that is returned by the `send` function on a channel.
        /// The future gets resolved with `None` when a value could be
        /// written to the channel.
        /// If the channel gets closed the send operation will fail, and the
        /// Future will resolve to `ChannelSendError(T)` and return the item
        /// to send.
        #[must_use = "futures do nothing unless polled"]
        pub struct ChannelSendFuture<T> {
            /// The LocalChannel that is associated with this ChannelSendFuture
            pub(crate) channel: Option<std::sync::Arc<dyn ChannelSendAccess<T>>>,
            /// Node for waiting on the channel
            pub(crate) wait_node: ListNode<SendWaitQueueEntry<T>>,
        }

        impl<T> core::fmt::Debug for ChannelSendFuture<T> {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                f.debug_struct("ChannelSendFuture")
                    .finish()
            }
        }

        impl<T> Future for ChannelSendFuture<T> {
            type Output = Result<(), ChannelSendError<T>>;

            fn poll(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), ChannelSendError<T>>> {
                // It might be possible to use Pin::map_unchecked here instead of the two unsafe APIs.
                // However this didn't seem to work for some borrow checker reasons

                // Safety: The next operations are safe, because Pin promises us that
                // the address of the wait queue entry inside ChannelSendFuture is stable,
                // and we don't move any fields inside the future until it gets dropped.
                let mut_self: &mut ChannelSendFuture<T> = unsafe {
                    Pin::get_unchecked_mut(self)
                };

                let channel = mut_self.channel.take().expect(
                    "polled ChannelSendFuture after completion");

                let send_res = unsafe {
                    channel.try_send(&mut mut_self.wait_node, cx)
                };

                match send_res.0 {
                    Poll::Ready(()) => {
                        // Value has been transmitted or channel was closed
                        match send_res.1 {
                            Some(v) => {
                                // Channel must have been closed
                                Poll::Ready(Err(ChannelSendError(v)))
                            },
                            None => Poll::Ready(Ok(()))
                        }
                    },
                    Poll::Pending => {
                        mut_self.channel = Some(channel);
                        Poll::Pending
                    },
                }
            }
        }

        impl<T> FusedFuture for ChannelSendFuture<T> {
            fn is_terminated(&self) -> bool {
                self.channel.is_none()
            }
        }

        impl<T> Drop for ChannelSendFuture<T> {
            fn drop(&mut self) {
                // If this ChannelSendFuture has been polled and it was added to the
                // wait queue at the channel, it must be removed before dropping.
                // Otherwise the channel would access invalid memory.
                if let Some(channel) = &self.channel {
                    channel.remove_send_waiter(&mut self.wait_node);
                }
            }
        }
    }
}

#[cfg(feature = "std")]
pub use self::if_alloc::*;