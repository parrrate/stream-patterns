use futures_util::{Future, SinkExt};

use crate::{Error, PatternStream};

pub(crate) struct OwnedClose<S>(pub(crate) S);

impl<S: PatternStream> Unpin for OwnedClose<S> {}

impl<S: PatternStream> Future for OwnedClose<S> {
    type Output = Result<(), Error<S>>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.poll_close_unpin(cx)
    }
}
