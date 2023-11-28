use std::{convert::Infallible, task::Poll};

use async_channel::{bounded, Receiver, Sender};
use futures_util::{SinkExt, Stream, StreamExt};

use crate::{
    promise::QPromise,
    push_stream::{PushStreams, Unlock},
    Done, MaybePoll, PatternStream, StatePoll,
};

enum PushState<S: PatternStream> {
    Pending,
    Msg(S::Msg, QPromise),
    Stream(S, Sender<Unlock>),
    Readying(S::Msg, QPromise, S, Sender<Unlock>),
    Sending(S::Msg, QPromise, S, Sender<Unlock>),
}

impl<S: PatternStream> Default for PushState<S> {
    fn default() -> Self {
        Self::Pending
    }
}

impl<S: PatternStream> PushState<S> {
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

struct Push<S: PatternStream> {
    streams: PushStreams<S>,
    msg_r: Receiver<(S::Msg, QPromise)>,
    state: PushState<S>,
}

impl<S: PatternStream> Unpin for Push<S> {}

impl<S: PatternStream> Stream for Push<S> {
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

impl<S: PatternStream> Push<S> {
    fn poll_msg(&mut self, cx: &mut std::task::Context<'_>) -> MaybePoll<Self> {
        match self.state.take() {
            PushState::Pending => match self.msg_r.poll_next_unpin(cx) {
                Poll::Ready(Some((msg, promise))) => {
                    self.state = PushState::Msg(msg, promise);
                    None
                }
                Poll::Ready(None) => Some(Poll::Ready(None)),
                Poll::Pending => Some(Poll::Pending),
            },
            PushState::Stream(stream, unlock_s) => match self.msg_r.poll_next_unpin(cx) {
                Poll::Ready(Some((msg, promise))) => {
                    self.state = PushState::Readying(msg, promise, stream, unlock_s);
                    None
                }
                Poll::Ready(None) => Some(Poll::Ready(None)),
                Poll::Pending => {
                    self.state = PushState::Stream(stream, unlock_s);
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
            (PushState::Pending, Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = PushState::Stream(stream, unlock_s);
                Some(Poll::Pending)
            }
            (PushState::Pending, Poll::Ready(None)) => Some(Poll::Ready(None)),
            (PushState::Pending, Poll::Pending) => Some(Poll::Pending),
            (PushState::Msg(msg, promise), Poll::Ready(Some((stream, unlock_s)))) => {
                self.state = PushState::Readying(msg, promise, stream, unlock_s);
                None
            }
            (PushState::Msg(_, _), Poll::Ready(None)) => Some(Poll::Ready(None)),
            (PushState::Msg(_, _), Poll::Pending) => Some(Poll::Pending),
            (PushState::Stream(_, _), Poll::Pending | Poll::Ready(None)) => Some(Poll::Pending),
            (_, Poll::Ready(Some(_))) => panic!("invalid state (stream yielded before unlock)"),
            (
                state @ (PushState::Readying(_, _, _, _) | PushState::Sending(_, _, _, _)),
                Poll::Pending | Poll::Ready(None),
            ) => {
                self.state = state;
                None
            }
        }
    }

    fn poll_state(&mut self, cx: &mut std::task::Context<'_>) -> StatePoll<Self> {
        match self.state.take() {
            PushState::Pending | PushState::Msg(_, _) | PushState::Stream(_, _) => {
                panic!("invalid state (pushing)")
            }
            PushState::Readying(msg, promise, mut stream, unlock_s) => {
                match stream.poll_ready_unpin(cx) {
                    Poll::Ready(Ok(())) => match stream.start_send_unpin(msg.clone()) {
                        Ok(()) => {
                            self.state = PushState::Sending(msg, promise, stream, unlock_s);
                            StatePoll::Continue
                        }
                        Err(e) => {
                            self.streams.done(Err(Some(e)));
                            let _ = unlock_s.try_send(Unlock::Unlock);
                            match self.streams.poll_next_unpin(cx) {
                                Poll::Ready(Some((stream, unlock_s))) => {
                                    self.state =
                                        PushState::Readying(msg, promise, stream, unlock_s);
                                    StatePoll::Continue
                                }
                                Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                                Poll::Pending => {
                                    self.state = PushState::Msg(msg, promise);
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
                                self.state = PushState::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = PushState::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = PushState::Readying(msg, promise, stream, unlock_s);
                        StatePoll::Poll(Poll::Pending)
                    }
                }
            }
            PushState::Sending(msg, promise, mut stream, unlock_s) => {
                match stream.poll_flush_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        promise.done();
                        self.streams.done(Ok(stream));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        StatePoll::Break
                    }
                    Poll::Ready(Err(e)) => {
                        self.streams.done(Err(Some(e)));
                        let _ = unlock_s.try_send(Unlock::Unlock);
                        match self.streams.poll_next_unpin(cx) {
                            Poll::Ready(Some((stream, unlock_s))) => {
                                self.state = PushState::Readying(msg, promise, stream, unlock_s);
                                StatePoll::Continue
                            }
                            Poll::Ready(None) => StatePoll::Poll(Poll::Ready(None)),
                            Poll::Pending => {
                                self.state = PushState::Msg(msg, promise);
                                StatePoll::Poll(Poll::Pending)
                            }
                        }
                    }
                    Poll::Pending => {
                        self.state = PushState::Sending(msg, promise, stream, unlock_s);
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

pub async fn push<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_r: Receiver<(S::Msg, QPromise)>,
) {
    let (unlock_s, unlock_r) = bounded(1);
    let _ = unlock_s.try_send(Unlock::Unlock);
    Push {
        streams: PushStreams::new(ready_r, done_s, unlock_r),
        msg_r,
        state: PushState::Pending,
    }
    .run()
    .await
}
