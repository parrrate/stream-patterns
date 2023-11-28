use std::{convert::Infallible, task::Poll};

use async_channel::{bounded, Receiver, Sender};
use futures_util::{SinkExt, Stream, StreamExt};

use crate::{
    promise::QPromise,
    push_stream::{PushStreams, Unlock},
    Done, MaybePoll, PatternStream, StatePoll,
};

enum ReqState<S: PatternStream> {
    Pending,
    Msg(S::Msg, QPromise<S::Msg>),
    Stream(S, Sender<Unlock>),
    Readying(S::Msg, QPromise<S::Msg>, S, Sender<Unlock>),
    Sending(S::Msg, QPromise<S::Msg>, S, Sender<Unlock>),
    Receiving(S::Msg, QPromise<S::Msg>, S, Sender<Unlock>),
}

impl<S: PatternStream> Default for ReqState<S> {
    fn default() -> Self {
        Self::Pending
    }
}

impl<S: PatternStream> ReqState<S> {
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

struct Req<S: PatternStream> {
    streams: PushStreams<S>,
    request_r: Receiver<(S::Msg, QPromise<S::Msg>)>,
    state: ReqState<S>,
}

impl<S: PatternStream> Unpin for Req<S> {}

impl<S: PatternStream> Stream for Req<S> {
    type Item = Infallible;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(poll) = self.poll_all(cx) {
                return poll;
            }
        }
    }
}

impl<S: PatternStream> Req<S> {
    fn poll_msg(&mut self, cx: &mut std::task::Context<'_>) -> MaybePoll<Self> {
        match self.state.take() {
            ReqState::Pending => match self.request_r.poll_next_unpin(cx) {
                Poll::Ready(Some((msg, promise))) => {
                    self.state = ReqState::Msg(msg, promise);
                    None
                }
                Poll::Ready(None) => Some(Poll::Ready(None)),
                Poll::Pending => Some(Poll::Pending),
            },
            ReqState::Stream(stream, unlock_s) => match self.request_r.poll_next_unpin(cx) {
                Poll::Ready(Some((msg, promise))) => {
                    self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                    None
                }
                Poll::Ready(None) => Some(Poll::Ready(None)),
                Poll::Pending => {
                    self.state = ReqState::Stream(stream, unlock_s);
                    Some(Poll::Pending)
                }
            },
            state => {
                self.state = state;
                None
            }
        }
    }

    fn poll_stream(&mut self, stream_poll: Poll<Option<(S, Sender<Unlock>)>>) -> MaybePoll<Self> {
        match (self.state.take(), stream_poll) {
            (ReqState::Pending, Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = ReqState::Stream(stream, unlock_s);
                Some(Poll::Pending)
            }
            (ReqState::Pending, Poll::Ready(None)) => Some(Poll::Ready(None)),
            (ReqState::Pending, Poll::Pending) => Some(Poll::Pending),
            (ReqState::Msg(msg, promise), Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                None
            }
            (ReqState::Msg(_, _), Poll::Ready(None)) => Some(Poll::Ready(None)),
            (ReqState::Msg(_, _), Poll::Pending) => Some(Poll::Pending),
            (ReqState::Stream(_, _), Poll::Pending | Poll::Ready(None)) => Some(Poll::Pending),
            (_, Poll::Ready(Some(_))) => panic!("invalid state (stream yielded before unlock)"),
            (
                state @ (ReqState::Readying(_, _, _, _)
                | ReqState::Sending(_, _, _, _)
                | ReqState::Receiving(_, _, _, _)),
                Poll::Pending | Poll::Ready(None),
            ) => {
                self.state = state;
                None
            }
        }
    }

    fn poll_state(&mut self, cx: &mut std::task::Context<'_>) -> StatePoll<Self> {
        match self.state.take() {
            ReqState::Pending | ReqState::Msg(_, _) | ReqState::Stream(_, _) => {
                panic!("invalid state (requesting)")
            }
            ReqState::Readying(msg, promise, mut stream, unlock_s) => {
                match stream.poll_ready_unpin(cx) {
                    Poll::Ready(Ok(())) => match stream.start_send_unpin(msg.clone()) {
                        Ok(()) => {
                            self.state = ReqState::Sending(msg, promise, stream, unlock_s);
                            StatePoll::Continue
                        }
                        Err(e) => {
                            self.streams.done(Err(Some(e)));
                            let _ = unlock_s.try_send(Unlock::Unlock);
                            match self.streams.poll_next_unpin(cx) {
                                Poll::Ready(Some((stream, unlock_s))) => {
                                    self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                                    StatePoll::Continue
                                }
                                Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                                Poll::Pending => {
                                    self.state = ReqState::Msg(msg, promise);
                                    StatePoll::Poll(Poll::Pending)
                                }
                            }
                        }
                    },
                    Poll::Ready(Err(e)) => {
                        self.streams.done(Err(Some(e)));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = ReqState::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = ReqState::Sending(msg, promise, stream, unlock_s);
                        StatePoll::Poll(Poll::Pending)
                    }
                }
            }
            ReqState::Sending(msg, promise, mut stream, unlock_s) => {
                match stream.poll_flush_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        self.state = ReqState::Receiving(msg, promise, stream, unlock_s);
                        StatePoll::Continue
                    }
                    Poll::Ready(Err(e)) => {
                        self.streams.done(Err(Some(e)));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = ReqState::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = ReqState::Sending(msg, promise, stream, unlock_s);
                        StatePoll::Poll(Poll::Pending)
                    }
                }
            }
            ReqState::Receiving(msg, promise, mut stream, unlock_s) => {
                match stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        promise.resolve(msg);
                        self.streams.done(Ok(stream));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        StatePoll::Break
                    }
                    Poll::Ready(Some(Err(e))) => {
                        self.streams.done(Err(Some(e)));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = ReqState::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Ready(None) => {
                        self.streams.done(Err(None));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = ReqState::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = ReqState::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = ReqState::Receiving(msg, promise, stream, unlock_s);
                        StatePoll::Poll(Poll::Pending)
                    }
                }
            }
        }
    }

    fn poll_state_loop(&mut self, cx: &mut std::task::Context<'_>) -> MaybePoll<Self> {
        loop {
            match self.poll_state(cx) {
                StatePoll::Poll(poll) => break Some(poll),
                StatePoll::Continue => {}
                StatePoll::Break => break None,
            }
        }
    }

    fn poll_all(&mut self, cx: &mut std::task::Context<'_>) -> MaybePoll<Self> {
        let stream_poll = self.streams.poll_next_unpin(cx);
        if let Some(poll) = self.poll_msg(cx) {
            return Some(poll);
        }
        if let Some(poll) = self.poll_stream(stream_poll) {
            return Some(poll);
        }
        if let Some(poll) = self.poll_state_loop(cx) {
            return Some(poll);
        }
        None
    }

    async fn run(&mut self) {
        while self.next().await.is_some() {}
    }
}

pub async fn req<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    request_r: Receiver<(S::Msg, QPromise<S::Msg>)>,
) {
    let (unlock_s, unlock_r) = bounded(1);
    let _ = unlock_s.try_send(Unlock::Unlock);
    Req {
        streams: PushStreams::new(ready_r, done_s, unlock_r),
        request_r,
        state: ReqState::Pending,
    }
    .run()
    .await
}
