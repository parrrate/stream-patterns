use std::task::Poll;

use async_channel::{Receiver, Sender};
use futures_util::{
    stream::{FuturesUnordered, SelectAll},
    SinkExt, Stream, StreamExt,
};

use crate::{
    promise::{QPromise, RequestSender},
    Done, PatternStream,
};

struct RepStream<S: PatternStream> {
    stream: Option<S>,
    done_s: Sender<Done<S>>,
}

impl<S: PatternStream> Unpin for RepStream<S> {}

impl<S: PatternStream> Stream for RepStream<S> {
    type Item = (S, S::Msg);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.stream.take() {
            Some(mut stream) => match stream.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(msg))) => Poll::Ready(Some((stream, msg))),
                Poll::Ready(Some(Err(e))) => {
                    let _ = self.done_s.try_send(Some(e));
                    Poll::Ready(None)
                }
                Poll::Ready(None) => {
                    let _ = self.done_s.try_send(None);
                    Poll::Ready(None)
                }
                Poll::Pending => {
                    self.stream = Some(stream);
                    Poll::Pending
                }
            },
            None => Poll::Ready(None),
        }
    }
}

struct Rep<S: PatternStream> {
    select: SelectAll<RepStream<S>>,
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
}

impl<S: PatternStream> Unpin for Rep<S> {}

impl<S: PatternStream> Stream for Rep<S> {
    type Item = (S, S::Msg);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(stream)) = self.ready_r.poll_next_unpin(cx) {
            self.add(stream);
        }
        match self.select.poll_next_unpin(cx) {
            Poll::Ready(None) if !self.ready_r.is_closed() => Poll::Pending,
            poll => poll,
        }
    }
}

impl<S: PatternStream> Rep<S> {
    fn add(&mut self, stream: S) {
        let stream = RepStream {
            stream: Some(stream),
            done_s: self.done_s.clone(),
        };
        self.select.push(stream);
    }

    async fn close(&mut self) {
        let mut futures = FuturesUnordered::new();
        for stream in self.select.iter_mut() {
            if let Some(ref mut stream) = stream.stream {
                futures.push(stream.close());
            }
        }
        while futures.next().await.is_some() {}
        drop(futures);
        self.select.clear();
    }

    async fn run(&mut self, request_s: Sender<(S::Msg, QPromise<S::Msg>)>) {
        while let Some((mut stream, msg)) = self.next().await {
            let msg = match request_s.request(msg).await.ok() {
                Some(msg) => msg,
                None => {
                    let r = stream.close().await;
                    let _ = self.done_s.send(r.err()).await;
                    continue;
                }
            };
            match stream.send(msg).await {
                Ok(_) => {
                    self.add(stream);
                }
                Err(e) => {
                    let _ = self.done_s.send(Some(e)).await;
                }
            }
        }
        self.close().await;
    }
}

pub async fn rep<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    request_s: Sender<(S::Msg, QPromise<S::Msg>)>,
) {
    Rep {
        select: SelectAll::new(),
        ready_r,
        done_s,
    }
    .run(request_s)
    .await
}
