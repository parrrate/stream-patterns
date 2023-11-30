use std::{convert::Infallible, task::Poll};

use async_channel::{bounded, Receiver, Sender};
use futures_util::{SinkExt, Stream, StreamExt};

use crate::{
    promise::QPromise,
    push_stream::{PushStreams, Unlock},
    Done, MaybePoll, PatternStream, PrePoll, StatePoll,
};

enum State<S: PatternStream> {
    Pending,
    Msg(S::Msg, QPromise<S::Msg>),
    Stream(S, Sender<Unlock>),
    Readying(S::Msg, QPromise<S::Msg>, S, Sender<Unlock>),
    Sending(S::Msg, QPromise<S::Msg>, S, Sender<Unlock>),
    Receiving(S::Msg, QPromise<S::Msg>, S, Sender<Unlock>),
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

struct Req<S: PatternStream> {
    streams: PushStreams<S>,
    request_r: Receiver<(S::Msg, QPromise<S::Msg>)>,
    state: State<S>,
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
    fn poll_msg(&mut self, cx: &mut std::task::Context<'_>) -> PrePoll {
        match self.state.take() {
            State::Pending => match self.request_r.poll_next_unpin(cx) {
                Poll::Ready(Some((msg, promise))) => {
                    self.state = State::Msg(msg, promise);
                    PrePoll::Continue
                }
                Poll::Ready(None) => PrePoll::Break,
                Poll::Pending => PrePoll::Pending,
            },
            State::Stream(stream, unlock_s) => match self.request_r.poll_next_unpin(cx) {
                Poll::Ready(Some((msg, promise))) => {
                    self.state = State::Readying(msg, promise, stream, unlock_s);
                    PrePoll::Continue
                }
                Poll::Ready(None) => PrePoll::Break,
                Poll::Pending => {
                    self.state = State::Stream(stream, unlock_s);
                    PrePoll::Pending
                }
            },
            state => {
                self.state = state;
                PrePoll::Continue
            }
        }
    }

    fn poll_stream(&mut self, cx: &mut std::task::Context<'_>) -> PrePoll {
        match (self.state.take(), self.streams.poll_next_unpin(cx)) {
            (State::Pending, Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = State::Stream(stream, unlock_s);
                PrePoll::Continue
            }
            (State::Pending, Poll::Ready(None)) => PrePoll::Break,
            (State::Pending, Poll::Pending) => PrePoll::Pending,
            (State::Msg(msg, promise), Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = State::Readying(msg, promise, stream, unlock_s);
                PrePoll::Continue
            }
            (State::Msg(_, _), Poll::Ready(None)) => PrePoll::Break,
            (State::Msg(msg, promise), Poll::Pending) => {
                self.state = State::Msg(msg, promise);
                PrePoll::Pending
            }
            (State::Stream(stream, unlock_s), Poll::Pending | Poll::Ready(None)) => {
                self.state = State::Stream(stream, unlock_s);
                PrePoll::Pending
            }
            (_, Poll::Ready(Some(_))) => panic!("invalid state (stream yielded before unlock)"),
            (
                state @ (State::Readying(_, _, _, _)
                | State::Sending(_, _, _, _)
                | State::Receiving(_, _, _, _)),
                Poll::Pending | Poll::Ready(None),
            ) => {
                self.state = state;
                PrePoll::Continue
            }
        }
    }

    fn poll_state(&mut self, cx: &mut std::task::Context<'_>) -> StatePoll<Self> {
        match self.state.take() {
            State::Pending | State::Msg(_, _) | State::Stream(_, _) => {
                panic!("invalid state (requesting)")
            }
            State::Readying(msg, promise, mut stream, unlock_s) => {
                match stream.poll_ready_unpin(cx) {
                    Poll::Ready(Ok(())) => match stream.start_send_unpin(msg.clone()) {
                        Ok(()) => {
                            self.state = State::Sending(msg, promise, stream, unlock_s);
                            StatePoll::Continue
                        }
                        Err(e) => {
                            self.streams.done(Some(e));
                            let _ = unlock_s.try_send(Unlock::Unlock);
                            match self.streams.poll_next_unpin(cx) {
                                Poll::Ready(Some((stream, unlock_s))) => {
                                    self.state = State::Readying(msg, promise, stream, unlock_s);
                                    StatePoll::Continue
                                }
                                Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                                Poll::Pending => {
                                    self.state = State::Msg(msg, promise);
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
                                self.state = State::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = State::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = State::Sending(msg, promise, stream, unlock_s);
                        StatePoll::Poll(Poll::Pending)
                    }
                }
            }
            State::Sending(msg, promise, mut stream, unlock_s) => {
                match stream.poll_flush_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        self.state = State::Receiving(msg, promise, stream, unlock_s);
                        StatePoll::Continue
                    }
                    Poll::Ready(Err(e)) => {
                        self.streams.done(Some(e));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = State::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = State::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = State::Sending(msg, promise, stream, unlock_s);
                        StatePoll::Poll(Poll::Pending)
                    }
                }
            }
            State::Receiving(msg, promise, mut stream, unlock_s) => {
                match stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        promise.resolve(msg);
                        self.streams.add(stream);
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        StatePoll::Break
                    }
                    Poll::Ready(Some(Err(e))) => {
                        self.streams.done(Some(e));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = State::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = State::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Ready(None) => {
                        self.streams.done(None);
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = State::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = State::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = State::Receiving(msg, promise, stream, unlock_s);
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
        match (self.poll_stream(cx), self.poll_msg(cx)) {
            (PrePoll::Break, _) | (_, PrePoll::Break) => return Some(Poll::Ready(None)),
            (PrePoll::Pending, _) | (_, PrePoll::Pending) => return Some(Poll::Pending),
            _ => {}
        }
        if let Some(poll) = self.poll_state_loop(cx) {
            return Some(poll);
        }
        None
    }

    async fn run(&mut self) {
        while self.next().await.is_some() {}
        match self.state.take() {
            State::Pending | State::Msg(_, _) => {}
            State::Stream(stream, _)
            | State::Readying(_, _, stream, _)
            | State::Sending(_, _, stream, _)
            | State::Receiving(_, _, stream, _) => {
                let mut stream = stream;
                let _ = stream.close().await;
            }
        }
        self.streams.close().await;
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
        state: State::Pending,
    }
    .run()
    .await
}
