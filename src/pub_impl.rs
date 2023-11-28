use std::{
    convert::Infallible,
    task::{Poll, Waker},
};

use async_channel::{Receiver, Sender};
use futures_util::{
    stream::{FuturesUnordered, SelectAll},
    SinkExt, Stream, StreamExt,
};

use crate::{promise::QPromise, Done, PatternStream};

struct PubStream<S: PatternStream> {
    stream: Option<S>,
    done_s: Sender<Done<S>>,
    waker: Option<Waker>,
}

impl<S: PatternStream> Unpin for PubStream<S> {}

impl<S: PatternStream> Stream for PubStream<S> {
    type Item = Infallible;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            match self.stream {
                Some(ref mut stream) => match stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(_))) => {}
                    Poll::Ready(Some(Err(e))) => {
                        let _ = self.done_s.try_send(Some(e));
                        self.stream = None;
                        break Poll::Ready(None);
                    }
                    Poll::Ready(None) => {
                        let _ = self.done_s.try_send(None);
                        self.stream = None;
                        break Poll::Ready(None);
                    }
                    Poll::Pending => {
                        self.waker = Some(cx.waker().clone());
                        break Poll::Pending;
                    }
                },
                None => break Poll::Ready(None),
            }
        }
    }
}

impl<S: PatternStream> PubStream<S> {
    async fn r#pub(&mut self, msg: S::Msg) {
        if let Some(ref mut stream) = self.stream {
            if let Err(e) = stream.send(msg).await {
                let _ = self.done_s.try_send(Some(e));
                self.stream = None;
                if let Some(waker) = self.waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

struct Pub<S: PatternStream> {
    select: SelectAll<PubStream<S>>,
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_r: Receiver<(S::Msg, QPromise)>,
}

impl<S: PatternStream> Unpin for Pub<S> {}

impl<S: PatternStream> Stream for Pub<S> {
    type Item = (S::Msg, QPromise);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(stream)) = self.ready_r.poll_next_unpin(cx) {
            let done_s = self.done_s.clone();
            self.select.push(PubStream {
                stream: Some(stream),
                done_s,
                waker: None,
            });
        }
        while let Poll::Ready(Some(_)) = self.select.poll_next_unpin(cx) {}
        self.msg_r.poll_next_unpin(cx)
    }
}

impl<S: PatternStream> Pub<S> {
    async fn r#pub(&mut self, msg: S::Msg) {
        let mut futures = FuturesUnordered::new();
        for stream in self.select.iter_mut() {
            futures.push(stream.r#pub(msg.clone()));
        }
        while futures.next().await.is_some() {}
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

    async fn run(&mut self) {
        while let Some((msg, promise)) = self.next().await {
            self.r#pub(msg).await;
            promise.done();
        }
        self.close().await;
    }
}

pub async fn r#pub<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_r: Receiver<(S::Msg, QPromise)>,
) {
    Pub {
        select: SelectAll::new(),
        ready_r,
        done_s,
        msg_r,
    }
    .run()
    .await
}
