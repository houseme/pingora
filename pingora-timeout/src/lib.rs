// Copyright 2024 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![warn(clippy::all)]

//! A drop-in replacement of [tokio::time::timeout] which is much more efficient.
//!
//! Similar to [tokio::time::timeout] but more efficient on busy concurrent IOs where timeouts are
//! created and canceled very frequently.
//!
//! This crate provides the following optimizations
//! - The timeouts lazily initializes their timer when the Future is pending for the first time.
//! - There is no global lock for creating and cancelling timeouts.
//! - Timeout timers are rounded to the next 10ms tick and timers are shared across all timeouts with the same deadline.
//!
//! Benchmark:
//!
//! 438.302µs total, 4ns avg per iteration
//!
//! v.s. Tokio timeout():
//!
//! 10.716192ms total, 107ns avg per iteration
//!

pub mod fast_timeout;
pub mod timer;

pub use fast_timeout::fast_sleep as sleep;
pub use fast_timeout::fast_timeout as timeout;

use futures::future::BoxFuture;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{self, Poll};
use tokio::time::{sleep as tokio_sleep, Duration};

/// The interface to start a timeout
///
/// Users don't need to interact with this trait
pub trait ToTimeout {
    fn timeout(&self) -> BoxFuture<'static, ()>;
    fn create(d: Duration) -> Self;
}

/// The timeout generated by [tokio_timeout()].
///
/// Users don't need to interact with this object.
pub struct TokioTimeout(Duration);

impl ToTimeout for TokioTimeout {
    fn timeout(&self) -> BoxFuture<'static, ()> {
        Box::pin(tokio_sleep(self.0))
    }

    fn create(d: Duration) -> Self {
        TokioTimeout(d)
    }
}

/// The error type returned when the timeout is reached.
#[derive(Debug)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Timeout Elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// The [tokio::time::timeout] with just lazy timer initialization.
///
/// The timer is created the first time the `future` is pending. This avoids unnecessary timer
/// creation and cancellation on busy IOs with a good chance to be already ready (e.g., reading
/// data from TCP where the recv buffer already has a lot of data to read right away).
pub fn tokio_timeout<T>(duration: Duration, future: T) -> Timeout<T, TokioTimeout>
where
    T: Future,
{
    Timeout::<T, TokioTimeout>::new_with_delay(future, duration)
}

pin_project! {
    /// The timeout future returned by the timeout functions
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct Timeout<T, F> {
        #[pin]
        value: T,
        #[pin]
        delay: Option<BoxFuture<'static, ()>>,
        callback: F, // callback to create the timer
    }
}

impl<T, F> Timeout<T, F>
where
    F: ToTimeout,
{
    pub(crate) fn new_with_delay(value: T, d: Duration) -> Timeout<T, F> {
        Timeout {
            value,
            delay: None,
            callback: F::create(d),
        }
    }
}

impl<T, F> Future for Timeout<T, F>
where
    T: Future,
    F: ToTimeout,
{
    type Output = Result<T::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let mut me = self.project();

        // First, try polling the future
        if let Poll::Ready(v) = me.value.poll(cx) {
            return Poll::Ready(Ok(v));
        }

        let delay = me
            .delay
            .get_or_insert_with(|| Box::pin(me.callback.timeout()));

        match delay.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => Poll::Ready(Err(Elapsed {})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_timeout() {
        let fut = tokio_sleep(Duration::from_secs(1000));
        let to = timeout(Duration::from_secs(1), fut);
        assert!(to.await.is_err())
    }

    #[tokio::test]
    async fn test_instantly_return() {
        let fut = async { 1 };
        let to = timeout(Duration::from_secs(1), fut);
        assert_eq!(to.await.unwrap(), 1)
    }

    #[tokio::test]
    async fn test_delayed_return() {
        let fut = async {
            tokio_sleep(Duration::from_secs(1)).await;
            1
        };
        let to = timeout(Duration::from_secs(1000), fut);
        assert_eq!(to.await.unwrap(), 1)
    }
}
