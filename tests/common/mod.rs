use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_channel::mpsc::{channel, Receiver, SendError, Sender};
use futures_util::{Sink, Stream, StreamExt};

pub struct Channel<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

impl<T> Stream for Channel<T> {
    type Item = Result<T, SendError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_next_unpin(cx).map(|o| o.map(Ok))
    }
}

impl<T> Sink<T> for Channel<T> {
    type Error = SendError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sender.poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, msg: T) -> Result<(), Self::Error> {
        self.sender.start_send(msg)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // https://docs.rs/futures-channel/0.3.29/src/futures_channel/mpsc/sink_impl.rs.html#17-25
        match self.sender.poll_ready(cx) {
            Poll::Ready(Err(ref e)) if e.is_disconnected() => Poll::Ready(Ok(())),
            poll => poll,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // https://docs.rs/futures-channel/0.3.29/src/futures_channel/mpsc/sink_impl.rs.html#17-25
        self.sender.disconnect();
        Poll::Ready(Ok(()))
    }
}

pub fn channel_pair<T>(size: usize) -> (Channel<T>, Channel<T>) {
    let (sender0, receiver0) = channel(size);
    let (sender1, receiver1) = channel(size);
    (
        Channel {
            sender: sender0,
            receiver: receiver1,
        },
        Channel {
            sender: sender1,
            receiver: receiver0,
        },
    )
}

impl<T> Unpin for Channel<T> {}
