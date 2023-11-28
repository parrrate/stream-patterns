use std::task::Poll;

use async_channel::{bounded, Receiver, Sender};
use futures_util::{stream::SelectAll, Stream, StreamExt};

use crate::{Done, PatternStream};

pub(crate) enum Unlock {
    Unlock,
    Skip(Receiver<Unlock>),
}

struct PushStream<S: PatternStream> {
    stream: Option<S>,
    done_s: Sender<Done<S>>,
    unlock_r: Receiver<Unlock>,
    unlock_s: Sender<Unlock>,
}

impl<S: PatternStream> Unpin for PushStream<S> {}

impl<S: PatternStream> Stream for PushStream<S> {
    type Item = (S, Sender<Unlock>);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            match self.stream.take() {
                Some(mut stream) => match stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(_))) => {
                        self.stream = Some(stream);
                    }
                    Poll::Ready(Some(Err(e))) => {
                        let _ = self.done_s.try_send(Err(Some(e)));
                        let _ = self.unlock_s.try_send(Unlock::Skip(self.unlock_r.clone()));
                        break Poll::Ready(None);
                    }
                    Poll::Ready(None) => {
                        let _ = self.done_s.try_send(Err(None));
                        let _ = self.unlock_s.try_send(Unlock::Skip(self.unlock_r.clone()));
                        break Poll::Ready(None);
                    }
                    Poll::Pending => match self.unlock_r.poll_next_unpin(cx) {
                        Poll::Ready(Some(Unlock::Skip(unlock_r))) => {
                            self.stream = Some(stream);
                            self.unlock_r = unlock_r;
                        }
                        Poll::Ready(Some(Unlock::Unlock)) => {
                            break Poll::Ready(Some((stream, self.unlock_s.clone())));
                        }
                        Poll::Ready(None) => break Poll::Ready(None),
                        Poll::Pending => break Poll::Pending,
                    },
                },
                None => break Poll::Ready(None),
            }
        }
    }
}

pub(crate) struct PushStreams<S: PatternStream> {
    select: SelectAll<PushStream<S>>,
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    unlock_r: Receiver<Unlock>,
}

impl<S: PatternStream> Unpin for PushStreams<S> {}

impl<S: PatternStream> Stream for PushStreams<S> {
    type Item = (S, Sender<Unlock>);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(stream)) = self.ready_r.poll_next_unpin(cx) {
            self.add(stream);
        }
        self.select.poll_next_unpin(cx)
    }
}

impl<S: PatternStream> PushStreams<S> {
    fn add(&mut self, stream: S) {
        let (unlock_s, unlock_r) = bounded(1);
        let stream = PushStream {
            stream: Some(stream),
            done_s: self.done_s.clone(),
            unlock_r: self.unlock_r.clone(),
            unlock_s,
        };
        self.select.push(stream);
        self.unlock_r = unlock_r;
    }

    pub(crate) fn new(
        ready_r: Receiver<S>,
        done_s: Sender<Done<S>>,
        unlock_r: Receiver<Unlock>,
    ) -> Self {
        Self {
            select: SelectAll::new(),
            ready_r,
            done_s,
            unlock_r,
        }
    }

    pub(crate) fn done(&self, done: Done<S>) {
        let _ = self.done_s.try_send(done);
    }
}
