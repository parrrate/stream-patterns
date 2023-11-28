use std::task::Poll;

pub use async_channel::{Receiver, Sender};
use futures_util::{Sink, Stream};

pub use self::{
    pub_impl::r#pub, pull_impl::pull, push_impl::push, rep_impl::rep, req_impl::req, sub_impl::sub,
};

pub mod promise;
mod pub_impl;
mod pull_impl;
mod push_impl;
mod push_stream;
mod rep_impl;
mod req_impl;
mod sub_impl;

pub trait ResultStream: Stream<Item = Result<Self::Msg, Self::Err>> {
    type Msg: Clone;
    type Err;
}

impl<S, T: Clone, E> ResultStream for S
where
    S: Stream<Item = Result<T, E>>,
{
    type Msg = T;
    type Err = E;
}

pub trait PatternStream: ResultStream + Sink<Self::Msg, Error = Self::Err> + Unpin {}

impl<S> PatternStream for S where S: ResultStream + Sink<Self::Msg, Error = Self::Err> + Unpin {}

type Error<S> = <S as Sink<<S as ResultStream>::Msg>>::Error;

type Done<S> = Option<Error<S>>;

type MaybePoll<T> = Option<Poll<Option<<T as Stream>::Item>>>;

enum StatePoll<T: Stream> {
    Poll(Poll<Option<<T as Stream>::Item>>),
    Continue,
    Break,
}

#[derive(Debug)]
enum PrePoll {
    Pending,
    Break,
    Continue,
}
