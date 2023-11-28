use std::task::Poll;

use async_channel::{Receiver, Sender};
use futures_util::{stream::SelectAll, Stream, StreamExt};

use crate::{
    promise::{QPromise, QSender},
    Done, PatternStream,
};

struct SubStream<S: PatternStream> {
    stream: S,
    done_s: Sender<Done<S>>,
}

impl<S: PatternStream> Unpin for SubStream<S> {}

impl<S: PatternStream> Stream for SubStream<S> {
    type Item = S::Msg;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.stream.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(Some(msg)),
            Poll::Ready(Some(Err(e))) => {
                let _ = self.done_s.try_send(Some(e));
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                let _ = self.done_s.try_send(None);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

struct Sub<S: PatternStream> {
    select: SelectAll<SubStream<S>>,
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
}

impl<S: PatternStream> Unpin for Sub<S> {}

impl<S: PatternStream> Stream for Sub<S> {
    type Item = S::Msg;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(stream)) = self.ready_r.poll_next_unpin(cx) {
            let done_s = self.done_s.clone();
            self.select.push(SubStream { stream, done_s });
        }
        match self.select.poll_next_unpin(cx) {
            Poll::Ready(None) if !self.ready_r.is_closed() => Poll::Pending,
            poll => poll,
        }
    }
}

impl<S: PatternStream> Sub<S> {
    async fn run(&mut self, msg_s: Sender<(S::Msg, QPromise)>) {
        let msg_q = QSender::new(msg_s);
        while let Some(msg) = self.next().await {
            if msg_q.request(msg).await.is_err() {
                break;
            }
        }
    }
}

pub async fn sub<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_s: Sender<(S::Msg, QPromise)>,
) {
    Sub {
        select: SelectAll::new(),
        ready_r,
        done_s,
    }
    .run(msg_s)
    .await
}
