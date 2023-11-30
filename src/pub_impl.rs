use std::{
    convert::Infallible,
    task::{Poll, Waker},
};

use async_channel::{bounded, Receiver, Sender};
use futures_util::{
    stream::{FuturesUnordered, SelectAll},
    Future, Sink, SinkExt, Stream, StreamExt,
};

use crate::{promise::QPromise, Done, Error, PatternStream};

struct PubStream<S: PatternStream> {
    stream: Option<S>,
    done_s: Sender<Done<S>>,
    waker: Option<Waker>,
    readying: Option<Sender<Infallible>>,
    flushing: Option<Sender<Infallible>>,
    ready: bool,
}

impl<S: PatternStream> Unpin for PubStream<S> {}

impl<S: PatternStream> Stream for PubStream<S> {
    type Item = Infallible;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.stream.take() {
            Some(mut stream) => {
                let mut pending = false;
                if let Some(sender) = self.readying.take() {
                    match stream.poll_ready_unpin(cx) {
                        Poll::Ready(Ok(())) => {
                            self.ready = true;
                        }
                        Poll::Ready(Err(e)) => {
                            let _ = self.done_s.try_send(Some(e));
                            return Poll::Ready(None);
                        }
                        Poll::Pending => {
                            self.readying = Some(sender);
                            pending = true;
                        }
                    }
                }
                if let Some(sender) = self.flushing.take() {
                    match stream.poll_flush_unpin(cx) {
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(e)) => {
                            let _ = self.done_s.try_send(Some(e));
                            return Poll::Ready(None);
                        }
                        Poll::Pending => {
                            self.flushing = Some(sender);
                            pending = true;
                        }
                    }
                }
                if pending {
                    self.waker = Some(cx.waker().clone());
                    self.stream = Some(stream);
                    return Poll::Pending;
                }
                loop {
                    match stream.poll_next_unpin(cx) {
                        Poll::Ready(Some(Ok(_))) => {}
                        Poll::Ready(Some(Err(e))) => {
                            let _ = self.done_s.try_send(Some(e));
                            break Poll::Ready(None);
                        }
                        Poll::Ready(None) => {
                            let _ = self.done_s.try_send(None);
                            break Poll::Ready(None);
                        }
                        Poll::Pending => {
                            self.waker = Some(cx.waker().clone());
                            self.stream = Some(stream);
                            break Poll::Pending;
                        }
                    }
                }
            }
            None => Poll::Ready(None),
        }
    }
}

impl<S: PatternStream> PubStream<S> {
    fn start_send(&mut self, item: S::Msg) {
        if self.ready {
            if let Some(mut stream) = self.stream.take() {
                match stream.start_send_unpin(item) {
                    Ok(()) => {
                        self.stream = Some(stream);
                    }
                    Err(e) => {
                        let _ = self.done_s.try_send(Some(e));
                        self.wake()
                    }
                }
            }
            self.ready = false;
        }
    }

    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

struct PubClose<S>(S);

impl<S: PatternStream> Unpin for PubClose<S> {}

impl<S: PatternStream> Future for PubClose<S> {
    type Output = Result<(), Error<S>>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        self.0.poll_close_unpin(cx)
    }
}

struct Pub<S: PatternStream> {
    select: SelectAll<PubStream<S>>,
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    closing: FuturesUnordered<PubClose<S>>,
    readying: Option<Receiver<Infallible>>,
    flushing: Option<Receiver<Infallible>>,
}

impl<S: PatternStream> Unpin for Pub<S> {}

impl<S: PatternStream> Sink<S::Msg> for Pub<S> {
    type Error = Infallible;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        loop {
            if let Poll::Ready(()) = self.pre_poll(cx) {
                break Poll::Ready(Ok(()));
            }
            match self.readying.take() {
                Some(mut receiver) => match receiver.poll_next_unpin(cx) {
                    Poll::Ready(Some(i)) => match i {},
                    Poll::Ready(None) => break Poll::Ready(Ok(())),
                    Poll::Pending => {
                        self.readying = Some(receiver);
                        break Poll::Pending;
                    }
                },
                None => {
                    let (sender, receiver) = bounded(1);
                    self.readying = Some(receiver);
                    for stream in self.select.iter_mut() {
                        stream.readying = Some(sender.clone());
                        stream.wake();
                    }
                }
            }
        }
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: S::Msg) -> Result<(), Self::Error> {
        for stream in self.select.iter_mut() {
            stream.start_send(item.clone());
        }
        Ok(())
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        loop {
            if let Poll::Ready(()) = self.pre_poll(cx) {
                break Poll::Ready(Ok(()));
            }
            match self.flushing.take() {
                Some(mut receiver) => match receiver.poll_next_unpin(cx) {
                    Poll::Ready(Some(i)) => match i {},
                    Poll::Ready(None) => break Poll::Ready(Ok(())),
                    Poll::Pending => {
                        self.flushing = Some(receiver);
                        break Poll::Pending;
                    }
                },
                None => {
                    let (sender, receiver) = bounded(1);
                    self.flushing = Some(receiver);
                    for stream in self.select.iter_mut() {
                        stream.flushing = Some(sender.clone());
                        stream.wake();
                    }
                }
            }
        }
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        loop {
            if !self.select.is_empty() {
                for stream in std::mem::take(&mut self.select) {
                    if let Some(stream) = stream.stream {
                        self.closing.push(PubClose(stream));
                    }
                }
            } else if !self.closing.is_empty() {
                match self.closing.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(()))) => {
                        let _ = self.done_s.try_send(None);
                    }
                    Poll::Ready(Some(Err(e))) => {
                        let _ = self.done_s.try_send(Some(e));
                    }
                    Poll::Ready(None) => {}
                    Poll::Pending => break Poll::Pending,
                }
            } else {
                break Poll::Ready(Ok(()));
            }
        }
    }
}

impl<S: PatternStream> Pub<S> {
    fn pre_poll(&mut self, cx: &mut std::task::Context<'_>) -> Poll<()> {
        while let Poll::Ready(Some(stream)) = self.ready_r.poll_next_unpin(cx) {
            let done_s = self.done_s.clone();
            self.select.push(PubStream {
                stream: Some(stream),
                done_s,
                waker: None,
                readying: None,
                flushing: None,
                ready: false,
            });
        }
        match self.select.poll_next_unpin(cx) {
            Poll::Ready(Some(i)) => match i {},
            Poll::Ready(None) if self.ready_r.is_closed() => Poll::Ready(()),
            Poll::Ready(None) => Poll::Pending,
            Poll::Pending => Poll::Pending,
        }
    }

    fn msg<'a>(&'a mut self, msg_r: &'a mut Receiver<(S::Msg, QPromise)>) -> PubMsg<'a, S> {
        PubMsg { sink: self, msg_r }
    }

    async fn run(&mut self, mut msg_r: Receiver<(S::Msg, QPromise)>) {
        while let Some((msg, promise)) = self.msg(&mut msg_r).next().await {
            match self.send(msg).await {
                Ok(()) => {}
                Err(i) => match i {},
            }
            promise.done()
        }
        match self.close().await {
            Ok(()) => {}
            Err(i) => match i {},
        }
    }
}

struct PubMsg<'a, S: PatternStream> {
    sink: &'a mut Pub<S>,
    msg_r: &'a mut Receiver<(S::Msg, QPromise)>,
}

impl<'a, S: PatternStream> Unpin for PubMsg<'a, S> {}

impl<'a, S: PatternStream> Stream for PubMsg<'a, S> {
    type Item = (S::Msg, QPromise);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(()) = self.sink.pre_poll(cx) {
            return Poll::Ready(None);
        };
        self.msg_r.poll_next_unpin(cx)
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
        closing: FuturesUnordered::new(),
        readying: None,
        flushing: None,
    }
    .run(msg_r)
    .await
}
