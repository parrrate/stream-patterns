use std::{
    pin::pin,
    task::{Context, Poll},
};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, StreamExt};
use fuzzer::{Local, Runner};
use stream_patterns::{promise::qpromise, push};

mod common;

#[test]
fn push_one() {
    let (pusher, mut puller) = channel_pair(10);
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut pushing = push::<Channel<i32>>(ready_r, done_s, msg_r);
    let mut pushing = pin!(pushing);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(pushing.as_mut().poll(&mut cx).is_pending());
    ready_s.try_send(pusher).unwrap();
    let (promise, future) = qpromise();
    msg_s.try_send((426, promise)).unwrap();
    msg_s.close();
    assert!(pushing.as_mut().poll(&mut cx).is_ready());
    assert!(pin!(future).poll(&mut cx).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
    assert!(done_r.try_recv().is_err());
    assert_eq!(
        pin!(puller.next()).poll(&mut cx),
        Poll::Ready(Some(Ok(426)))
    );
}

fn push_one_seed(seed: u64) {
    let mut runner = Runner::new(seed);
    let (pusher, puller) = runner.channel();
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut pushing = pin!(push::<Local<i32>>(ready_r, done_s, msg_r));
    ready_s.try_send(pusher).unwrap();
    let (promise, future) = qpromise();
    msg_s.try_send((426, promise)).unwrap();
    runner.pending_in(100, pushing.as_mut());
    msg_s.close();
    runner.ready_in(100, pushing.as_mut());
    assert!(runner.poll(pin!(future)).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
    assert!(done_r.try_recv().is_err());
    assert_eq!(puller.recv(), Some(426));
}

#[test]
fn push_one_fuzz() {
    for i in 0..1000 {
        push_one_seed(i);
    }
}

fn push_two_seed(seed: u64) {
    let mut runner = Runner::new(seed);
    let (pusher0, puller0) = runner.channel();
    let (pusher1, puller1) = runner.channel();
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut pushing = pin!(push::<Local<i32>>(ready_r, done_s, msg_r));
    ready_s.try_send(pusher0).unwrap();
    ready_s.try_send(pusher1).unwrap();
    let (promise0, future0) = qpromise();
    msg_s.try_send((426, promise0)).unwrap();
    let (promise1, future1) = qpromise();
    msg_s.try_send((426, promise1)).unwrap();
    runner.pending_in(100, pushing.as_mut());
    msg_s.close();
    runner.ready_in(100, pushing.as_mut());
    assert!(runner.poll(pin!(future0)).is_ready());
    assert!(runner.poll(pin!(future1)).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
    assert!(done_r.try_recv().unwrap().is_none());
    assert!(done_r.try_recv().is_err());
    assert_eq!(puller0.recv(), Some(426));
    assert_eq!(puller1.recv(), Some(426));
}

#[test]
fn push_two_fuzz() {
    for i in 0..1000 {
        push_two_seed(i);
    }
}
