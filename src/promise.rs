//! One-way one-time communication
//!
//! See also:
//! * <https://docs.rs/pinky-swear/latest/pinky_swear/index.html>
//! * <https://docs.rs/futures/latest/futures/channel/oneshot/index.html>
//! * <https://docs.rs/tokio/latest/tokio/sync/oneshot/index.html>

use std::{fmt::Display, task::Poll};

use async_channel::{bounded, Receiver, Sender};
use futures_util::{Future, FutureExt, StreamExt};

#[derive(Debug, PartialEq, Eq)]
pub struct Error;

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "promise error")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub struct QPromise<T = ()> {
    sender: Sender<T>,
}

pub struct QFuture<T> {
    receiver: Receiver<T>,
}

impl<T> QPromise<T> {
    pub fn resolve(self, value: T) {
        let _ = self.sender.try_send(value);
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    pub fn ignore() -> Self {
        Self {
            sender: bounded(1).0,
        }
    }

    pub fn map<A, B>(f: impl Fn(A) -> B) -> impl Fn((A, Self)) -> (B, Self) {
        move |(a, promise)| (f(a), promise)
    }

    pub fn new(sender: Sender<T>) -> Self {
        Self { sender }
    }
}

impl QPromise {
    pub fn done(self) {
        self.resolve(())
    }
}

impl<T> QFuture<T> {
    pub fn new(receiver: Receiver<T>) -> Self {
        Self { receiver }
    }
}

impl<T> Unpin for QFuture<T> {}

impl<T> Future for QFuture<T> {
    type Output = Result<T>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        self.receiver
            .poll_next_unpin(cx)
            .map(|value| value.ok_or(Error))
    }
}

pub fn qpromise<T>() -> (QPromise<T>, QFuture<T>) {
    let (sender, receiver) = bounded(1);
    (QPromise { sender }, QFuture { receiver })
}

pub struct Request<'a, T, U> {
    send: Option<async_channel::Send<'a, (T, QPromise<U>)>>,
    future: QFuture<U>,
}

impl<'a, T, U> Unpin for Request<'a, T, U> {}

impl<'a, T, U> Future for Request<'a, T, U> {
    type Output = Result<U>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        if let Some(mut send) = self.send.take() {
            match send.poll_unpin(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(_)) => return Poll::Ready(Err(Error)),
                Poll::Pending => {
                    self.send = Some(send);
                    return Poll::Pending;
                }
            }
        }
        self.future.poll_unpin(cx)
    }
}

pub trait RequestSender {
    type T;
    type U;

    fn request(&self, msg: Self::T) -> Request<'_, Self::T, Self::U>;
}

impl<T, U> RequestSender for Sender<(T, QPromise<U>)> {
    type T = T;
    type U = U;

    fn request(&self, msg: Self::T) -> Request<'_, Self::T, Self::U> {
        let (promise, future) = qpromise();
        Request {
            send: Some(self.send((msg, promise))),
            future,
        }
    }
}

pub type QSender<T, U> = Sender<(T, QPromise<U>)>;
