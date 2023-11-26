use async_channel::{bounded, Receiver, RecvError, Sender};

pub struct QPromise<T = ()> {
    sender: Sender<T>,
}

pub struct QFuture<T> {
    receiver: Receiver<T>,
}

impl<T> QPromise<T> {
    pub fn resolve(self, value: T) {
        let _ = self.sender.try_send(value);
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

impl QPromise {
    pub fn done(self) {
        self.resolve(())
    }
}

impl<T> QFuture<T> {
    pub async fn wait(self) -> Result<T, RecvError> {
        self.receiver.recv().await
    }
}

pub fn qpromise<T>() -> (QPromise<T>, QFuture<T>) {
    let (sender, receiver) = bounded(1);
    (QPromise { sender }, QFuture { receiver })
}

/// Request-reply communication.
pub struct QSender<T, U = ()> {
    sender: Sender<(T, QPromise<U>)>,
}

impl<T, U> QSender<T, U> {
    pub fn new(sender: Sender<(T, QPromise<U>)>) -> Self {
        Self { sender }
    }

    pub async fn request(&self, msg: T) -> Result<U, RecvError> {
        let (promise, future) = qpromise();
        let _ = self.sender.send((msg, promise)).await;
        future.wait().await
    }
}
