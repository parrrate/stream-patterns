use std::task::{ready, Poll};

use async_channel::{bounded, Receiver, Sender};
use futures_util::{stream::FuturesUnordered, Sink, SinkExt, Stream, StreamExt};

use crate::{
    owned_close::OwnedClose,
    promise::QPromise,
    push_stream::{PushStreams, Unlock},
    Done, PatternStream,
};

enum State<S: PatternStream> {
    Pending,
    Msg(S::Msg),
    Stream(S, Sender<Unlock>),
    Readying(S::Msg, S, Sender<Unlock>),
    Sending(S::Msg, S, Sender<Unlock>),
}

impl<S: PatternStream> core::fmt::Display for State<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Pending => write!(f, "pending"),
            State::Msg(_) => write!(f, "msg"),
            State::Stream(_, _) => write!(f, "stream"),
            State::Readying(_, _, _) => write!(f, "readying"),
            State::Sending(_, _, _) => write!(f, "sending"),
        }
    }
}

impl<S: PatternStream> Default for State<S> {
    fn default() -> Self {
        Self::Pending
    }
}

impl<S: PatternStream> State<S> {
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

struct Push<S: PatternStream> {
    streams: PushStreams<S>,
    state: State<S>,
    closing: FuturesUnordered<OwnedClose<S>>,
}

impl<S: PatternStream> Unpin for Push<S> {}

enum PushError<S: PatternStream> {
    NoStreams(S::Msg),
    AlreadySending,
}

impl<S: PatternStream> Sink<S::Msg> for Push<S> {
    type Error = PushError<S>;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: S::Msg) -> Result<(), Self::Error> {
        match self.state.take() {
            State::Pending => {
                self.state = State::Msg(item);
                Ok(())
            }
            State::Msg(_) => Err(PushError::AlreadySending),
            State::Stream(stream, unlock_s) => {
                self.state = State::Readying(item, stream, unlock_s);
                Ok(())
            }
            State::Readying(_, _, _) => Err(PushError::AlreadySending),
            State::Sending(_, _, _) => Err(PushError::AlreadySending),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if let StatePoll::Poll(poll) = self.poll_stream(cx) {
            return poll;
        }
        loop {
            if let StatePoll::Poll(poll) = self.poll_state(cx) {
                break poll;
            }
        }
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        for stream in self.streams.drain() {
            self.closing.push(OwnedClose(stream));
        }
        loop {
            match self.closing.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(()))) => {
                    self.streams.done(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    self.streams.done(Some(e));
                }
                Poll::Ready(None) => break Poll::Ready(Ok(())),
                Poll::Pending => break Poll::Pending,
            }
        }
    }
}

impl<S: PatternStream> Push<S> {
    fn poll_stream(&mut self, cx: &mut std::task::Context<'_>) -> StatePoll<S> {
        match (self.state.take(), self.streams.poll_next_unpin(cx)) {
            (State::Pending, Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = State::Stream(stream, unlock_s);
                StatePoll::Poll(Poll::Ready(Ok(())))
            }
            (State::Pending, Poll::Pending | Poll::Ready(None)) => {
                StatePoll::Poll(Poll::Ready(Ok(())))
            }
            (State::Msg(msg), Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = State::Readying(msg, stream, unlock_s);
                StatePoll::Continue
            }
            (State::Msg(msg), Poll::Ready(None)) => {
                StatePoll::Poll(Poll::Ready(Err(PushError::NoStreams(msg))))
            }
            (State::Msg(msg), Poll::Pending) => {
                self.state = State::Msg(msg);
                StatePoll::Poll(Poll::Pending)
            }
            (State::Stream(stream, unlock_s), Poll::Pending | Poll::Ready(None)) => {
                self.state = State::Stream(stream, unlock_s);
                StatePoll::Poll(Poll::Ready(Ok(())))
            }
            (_, Poll::Ready(Some(_))) => panic!("invalid state (stream yielded before unlock)"),
            (
                state @ (State::Readying(_, _, _) | State::Sending(_, _, _)),
                Poll::Pending | Poll::Ready(None),
            ) => {
                self.state = state;
                StatePoll::Continue
            }
        }
    }

    fn poll_state(&mut self, cx: &mut std::task::Context<'_>) -> StatePoll<S> {
        match self.state.take() {
            state @ (State::Pending | State::Stream(_, _)) => {
                self.state = state;
                StatePoll::Poll(Poll::Ready(Ok(())))
            }
            State::Msg(msg) => match self.streams.poll_next_unpin(cx) {
                Poll::Ready(Some((stream, unlock_s))) => {
                    self.state = State::Readying(msg, stream, unlock_s);
                    StatePoll::Continue
                }
                Poll::Ready(None) => StatePoll::Poll(Poll::Ready(Err(PushError::NoStreams(msg)))),
                Poll::Pending => {
                    self.state = State::Msg(msg);
                    StatePoll::Poll(Poll::Pending)
                }
            },
            State::Readying(msg, mut stream, unlock_s) => match stream.poll_ready_unpin(cx) {
                Poll::Ready(Ok(())) => match stream.start_send_unpin(msg.clone()) {
                    Ok(()) => {
                        self.state = State::Sending(msg, stream, unlock_s);
                        StatePoll::Continue
                    }
                    Err(e) => {
                        self.streams.done(Some(e));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = State::Readying(msg, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => {
                                StatePoll::Poll(Poll::Ready(Err(PushError::NoStreams(msg))))
                            }
                            Poll::Pending => {
                                self.state = State::Msg(msg);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                },
                Poll::Ready(Err(e)) => {
                    self.streams.done(Some(e));
                    let _ = unlock_s.try_send(Unlock::Unlock);
                    match self.streams.poll_next_unpin(cx) {
                        Poll::Ready(Some((stream, unlock_s))) => {
                            self.state = State::Readying(msg, stream, unlock_s);
                            StatePoll::Continue
                        }
                        Poll::Ready(None) => {
                            StatePoll::Poll(Poll::Ready(Err(PushError::NoStreams(msg))))
                        }
                        Poll::Pending => {
                            self.state = State::Msg(msg);
                            StatePoll::Poll(Poll::Pending)
                        }
                    }
                }
                Poll::Pending => {
                    self.state = State::Readying(msg, stream, unlock_s);
                    StatePoll::Poll(Poll::Pending)
                }
            },
            State::Sending(msg, mut stream, unlock_s) => match stream.poll_flush_unpin(cx) {
                Poll::Ready(Ok(())) => {
                    self.streams.add(stream);
                    let _ = unlock_s.try_send(Unlock::Unlock);
                    StatePoll::Poll(Poll::Ready(Ok(())))
                }
                Poll::Ready(Err(e)) => {
                    self.streams.done(Some(e));
                    let _ = unlock_s.try_send(Unlock::Unlock);
                    match self.streams.poll_next_unpin(cx) {
                        Poll::Ready(Some((stream, unlock_s))) => {
                            self.state = State::Readying(msg, stream, unlock_s);
                            StatePoll::Continue
                        }
                        Poll::Ready(None) => {
                            StatePoll::Poll(Poll::Ready(Err(PushError::NoStreams(msg))))
                        }
                        Poll::Pending => {
                            self.state = State::Msg(msg);
                            StatePoll::Poll(Poll::Pending)
                        }
                    }
                }
                Poll::Pending => {
                    self.state = State::Sending(msg, stream, unlock_s);
                    StatePoll::Poll(Poll::Pending)
                }
            },
        }
    }

    fn msg<'a>(&'a mut self, msg_r: &'a mut Receiver<(S::Msg, QPromise)>) -> PushMsg<'a, S> {
        PushMsg { sink: self, msg_r }
    }

    async fn run(&mut self, mut msg_r: Receiver<(S::Msg, QPromise)>) {
        while let Some((msg, promise)) = self.msg(&mut msg_r).next().await {
            if self.send(msg).await.is_err() {
                break;
            }
            promise.done()
        }
        let _ = self.close().await;
    }
}

enum StatePoll<S: PatternStream> {
    Poll(Poll<Result<(), PushError<S>>>),
    Continue,
}

struct PushMsg<'a, S: PatternStream> {
    sink: &'a mut Push<S>,
    msg_r: &'a mut Receiver<(S::Msg, QPromise)>,
}

impl<'a, S: PatternStream> Unpin for PushMsg<'a, S> {}

impl<'a, S: PatternStream> Stream for PushMsg<'a, S> {
    type Item = (S::Msg, QPromise);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Err(_)) = self.sink.poll_flush_unpin(cx) {
            return Poll::Ready(None);
        }
        self.msg_r.poll_next_unpin(cx)
    }
}

pub async fn push<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_r: Receiver<(S::Msg, QPromise)>,
) {
    let (unlock_s, unlock_r) = bounded(1);
    let _ = unlock_s.try_send(Unlock::Unlock);
    Push {
        streams: PushStreams::new(ready_r, done_s, unlock_r),
        state: State::Pending,
        closing: FuturesUnordered::new(),
    }
    .run(msg_r)
    .await
}
