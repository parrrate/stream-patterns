use std::{
    convert::Infallible,
    task::{Poll, Waker},
};

pub use async_channel::{Receiver, Sender};
use futures_util::{
    stream::{FuturesUnordered, SelectAll},
    Sink, SinkExt, Stream, StreamExt,
};
use promise::{QPromise, QSender};

pub mod promise;

pub trait PatternStream:
    Stream<Item = Result<Self::Msg, Self::Error>> + Sink<Self::Msg> + Unpin
{
    type Msg: Clone;
}

type Error<S> = <S as Sink<<S as PatternStream>::Msg>>::Error;

type MaybeError<S> = Option<Error<S>>;

type Done<S> = Result<S, MaybeError<S>>;

pub async fn push<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_r: Receiver<(S::Msg, QPromise)>,
) {
    while let Ok((msg, promise)) = msg_r.recv().await {
        loop {
            let Ok(mut stream) = ready_r.recv().await else {
                return;
            };
            match stream.send(msg.clone()).await {
                Ok(_) => {
                    let _ = done_s.send(Ok(stream)).await;
                    break;
                }
                Err(e) => {
                    let _ = done_s.send(Err(Some(e))).await;
                }
            }
        }
        promise.done();
    }
}

pub async fn pull<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    payload_s: Sender<(S::Msg, QPromise)>,
) {
    sub(ready_r, done_s, payload_s).await
}

fn rotate<T, E>(result: Option<Result<T, E>>) -> Result<T, Option<E>> {
    match result {
        Some(Ok(t)) => Ok(t),
        Some(Err(e)) => Err(Some(e)),
        None => Err(None),
    }
}

async fn do_req<S: PatternStream>(stream: &mut S, msg: S::Msg) -> Result<S::Msg, Option<Error<S>>> {
    stream.send(msg).await?;
    rotate(stream.next().await)
}

pub async fn req<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    request_r: Receiver<(S::Msg, QPromise<S::Msg>)>,
) {
    while let Ok((msg, promise)) = request_r.recv().await {
        let msg = loop {
            let Ok(mut stream) = ready_r.recv().await else {
                return;
            };
            match do_req(&mut stream, msg.clone()).await {
                Ok(msg) => {
                    let _ = done_s.send(Ok(stream)).await;
                    break msg;
                }
                Err(e) => {
                    let _ = done_s.send(Err(e)).await;
                }
            }
        };
        promise.resolve(msg);
    }
}

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
                    let _ = self.done_s.try_send(Err(Some(e)));
                    Poll::Ready(None)
                }
                Poll::Ready(None) => {
                    let _ = self.done_s.try_send(Err(None));
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
            let done_s = self.done_s.clone();
            self.select.push(RepStream {
                stream: Some(stream),
                done_s,
            });
        }
        match self.select.poll_next_unpin(cx) {
            Poll::Ready(None) if !self.ready_r.is_closed() => Poll::Pending,
            poll => poll,
        }
    }
}

impl<S: PatternStream> Rep<S> {
    async fn run(&mut self, request_s: Sender<(S::Msg, QPromise<S::Msg>)>) {
        let request_q = QSender::new(request_s);
        while let Some((mut stream, msg)) = self.next().await {
            let msg = match request_q.request(msg).await {
                Ok(msg) => msg,
                Err(_) => {
                    let _ = self.done_s.send(Err(None)).await;
                    continue;
                }
            };
            match stream.send(msg).await {
                Ok(_) => {
                    let _ = self.done_s.send(Ok(stream)).await;
                }
                Err(e) => {
                    let _ = self.done_s.send(Err(Some(e))).await;
                }
            }
        }
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
                let _ = self.done_s.try_send(Err(Some(e)));
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                let _ = self.done_s.try_send(Err(None));
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
    async fn run(&mut self, payload_s: Sender<(S::Msg, QPromise)>) {
        let payload_q = QSender::new(payload_s);
        while let Some(msg) = self.next().await {
            if payload_q.request(msg).await.is_err() {
                break;
            }
        }
    }
}

pub async fn sub<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    payload_s: Sender<(S::Msg, QPromise)>,
) {
    Sub {
        select: SelectAll::new(),
        ready_r,
        done_s,
    }
    .run(payload_s)
    .await
}

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
        match self.stream {
            Some(ref mut stream) => match stream.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(_))) => {
                    let _ = self.done_s.try_send(Err(None));
                    self.stream = None;
                    Poll::Ready(None)
                }
                Poll::Ready(Some(Err(e))) => {
                    let _ = self.done_s.try_send(Err(Some(e)));
                    self.stream = None;
                    Poll::Ready(None)
                }
                Poll::Ready(None) => {
                    let _ = self.done_s.try_send(Err(None));
                    self.stream = None;
                    Poll::Ready(None)
                }
                Poll::Pending => {
                    self.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            },
            None => Poll::Ready(None),
        }
    }
}

impl<S: PatternStream> PubStream<S> {
    async fn r#pub(&mut self, msg: S::Msg) {
        if let Some(ref mut stream) = self.stream {
            if let Err(e) = stream.send(msg).await {
                let _ = self.done_s.try_send(Err(Some(e)));
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
    payload_r: Receiver<(S::Msg, QPromise)>,
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
        self.payload_r.poll_next_unpin(cx)
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

    async fn run(&mut self) {
        while let Some((msg, promise)) = self.next().await {
            self.r#pub(msg).await;
            promise.done();
        }
    }
}

pub async fn r#pub<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    payload_r: Receiver<(S::Msg, QPromise)>,
) {
    Pub {
        select: SelectAll::new(),
        ready_r,
        done_s,
        payload_r,
    }
    .run()
    .await
}
