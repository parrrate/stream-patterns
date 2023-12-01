use std::task::Poll;

use async_channel::{unbounded, Receiver, Sender};
use futures_util::{Future, Sink, Stream, StreamExt};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};

enum Extra {}

pub struct Local<T> {
    sender: Option<Sender<T>>,
    receiver: Receiver<T>,
    ready_extra: Option<Receiver<Extra>>,
    flush_extra: Option<Receiver<Extra>>,
    close_extra: Option<Receiver<Extra>>,
    next_extra: Option<Receiver<Extra>>,
    extras: Sender<Sender<Extra>>,
    rng: StdRng,
}

pub struct Remote<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
}

impl<T> Remote<T> {
    pub fn send(&self, value: T) {
        let _ = self.sender.try_send(value);
    }

    pub fn recv(&self) -> Option<T> {
        self.receiver.try_recv().ok()
    }
}

struct PopSet<T> {
    values: Vec<Option<T>>,
    rng: StdRng,
}

impl<T> PopSet<T> {
    fn push(&mut self, value: T) {
        self.values.push(Some(value))
    }

    fn pop(&mut self) -> Option<T> {
        if self.values.is_empty() {
            return None;
        }
        let i = self.rng.gen_range(0..self.values.len());
        if let Some(value) = self.values[i].take() {
            return Some(value);
        }
        let values = std::mem::take(&mut self.values)
            .into_iter()
            .filter(Option::is_some);
        self.values.extend(values);
        if self.values.is_empty() {
            return None;
        }
        let i = self.rng.gen_range(0..self.values.len());
        Some(self.values[i].take().expect("filtered to be some"))
    }
}

pub struct Runner {
    set: PopSet<Sender<Extra>>,
    receiver: Receiver<Sender<Extra>>,
    sender: Sender<Sender<Extra>>,
}

impl Runner {
    pub fn new(seed: u64) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            set: PopSet {
                values: Vec::new(),
                rng: StdRng::seed_from_u64(seed),
            },
            receiver,
            sender,
        }
    }

    pub fn step(&mut self) -> bool {
        while let Ok(sender) = self.receiver.try_recv() {
            self.set.push(sender);
        }
        self.set.pop().is_some()
    }

    pub fn channel<T>(&mut self) -> (Local<T>, Remote<T>) {
        let (local_sender, foreign_receiver) = unbounded();
        let (foreign_sender, local_receiver) = unbounded();
        let rng = StdRng::seed_from_u64(self.set.rng.next_u64());
        let local = Local {
            sender: Some(local_sender),
            receiver: local_receiver,
            ready_extra: None,
            flush_extra: None,
            close_extra: None,
            next_extra: None,
            extras: self.sender.clone(),
            rng,
        };
        let foreign = Remote {
            sender: foreign_sender,
            receiver: foreign_receiver,
        };
        (local, foreign)
    }

    pub fn event(&mut self) -> Event {
        let rng = StdRng::seed_from_u64(self.set.rng.next_u64());
        Event {
            extra: None,
            extras: self.sender.clone(),
            rng,
        }
    }
}

pub enum TestError {
    Closed,
}

impl<T> Local<T> {
    fn pre_poll(
        &mut self,
        cx: &mut std::task::Context<'_>,
        mut extra: Option<Receiver<Extra>>,
    ) -> Option<Receiver<Extra>> {
        loop {
            if let Some(mut receiver) = extra.take() {
                match receiver.poll_next_unpin(cx) {
                    Poll::Ready(Some(extra)) => match extra {},
                    Poll::Ready(None) => {}
                    Poll::Pending => {
                        break Some(receiver);
                    }
                }
            }
            if self.rng.gen_ratio(1, 2) {
                break None;
            }
            let (sender, receiver) = unbounded();
            if self.extras.try_send(sender).is_err() {
                break None;
            }
            extra = Some(receiver);
        }
    }
}

impl<T> Unpin for Local<T> {}

impl<T> Stream for Local<T> {
    type Item = Result<T, TestError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let extra = self.next_extra.take();
        if let Some(receiver) = self.pre_poll(cx, extra) {
            self.next_extra = Some(receiver);
            return Poll::Pending;
        }
        self.receiver.poll_next_unpin(cx).map(|t| t.map(Ok))
    }
}

impl<T> Sink<T> for Local<T> {
    type Error = TestError;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        let extra = self.ready_extra.take();
        if let Some(receiver) = self.pre_poll(cx, extra) {
            self.ready_extra = Some(receiver);
            return Poll::Pending;
        }
        self.sender.as_ref().ok_or(TestError::Closed)?;
        Poll::Ready(Ok(()))
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let sender = self.sender.as_ref().ok_or(TestError::Closed)?;
        sender.try_send(item).map_err(|_| TestError::Closed)?;
        Ok(())
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        let extra = self.flush_extra.take();
        if let Some(receiver) = self.pre_poll(cx, extra) {
            self.flush_extra = Some(receiver);
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        let extra = self.close_extra.take();
        if let Some(receiver) = self.pre_poll(cx, extra) {
            self.close_extra = Some(receiver);
            return Poll::Pending;
        }
        self.sender.take();
        Poll::Ready(Ok(()))
    }
}

pub struct Event {
    extra: Option<Receiver<Extra>>,
    extras: Sender<Sender<Extra>>,
    rng: StdRng,
}

impl Unpin for Event {}

impl Future for Event {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        loop {
            if let Some(mut receiver) = self.extra.take() {
                match receiver.poll_next_unpin(cx) {
                    Poll::Ready(Some(extra)) => match extra {},
                    Poll::Ready(None) => {}
                    Poll::Pending => {
                        self.extra = Some(receiver);
                        break Poll::Pending;
                    }
                }
            }
            if self.rng.gen_ratio(1, 2) {
                break Poll::Ready(());
            }
            let (sender, receiver) = unbounded();
            if self.extras.try_send(sender).is_err() {
                break Poll::Ready(());
            }
            self.extra = Some(receiver);
        }
    }
}
