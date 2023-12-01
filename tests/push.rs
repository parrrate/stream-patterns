use std::{
    pin::pin,
    task::{Context, Poll},
};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, StreamExt};
use fuzzer::{AtMost, Local, Runner};
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
    assert!(pin!(future.wait()).poll(&mut cx).is_ready());
    assert!(done_r.try_recv().is_err());
    assert_eq!(
        pin!(puller.next()).poll(&mut cx),
        Poll::Ready(Some(Ok(426)))
    );
}

fn push_one_seed(seed: u64) {
    let mut runner = Runner::new(seed);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    let (pusher, puller) = runner.channel();
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut pushing = pin!(push::<Local<i32>>(ready_r, done_s, msg_r));
    ready_s.try_send(pusher).unwrap();
    let (promise, future) = qpromise();
    msg_s.try_send((426, promise)).unwrap();
    msg_s.close();
    for _ in AtMost(100) {
        if pushing.as_mut().poll(&mut cx).is_ready() {
            break;
        }
        assert!(runner.step())
    }
    assert!(pin!(future.wait()).poll(&mut cx).is_ready());
    assert!(done_r.try_recv().is_err());
    assert_eq!(puller.recv(), Some(426));
}

#[test]
fn push_one_fuzz() {
    for i in 0..1000 {
        push_one_seed(i);
    }
}
