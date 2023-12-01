use std::{
    pin::pin,
    task::{Context, Poll},
};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, StreamExt};
use fuzzer::{Local, Runner};
use stream_patterns::{promise::qpromise, r#pub};

mod common;

#[test]
fn pub_two() {
    let (publisher0, mut subscriber0) = channel_pair(10);
    let (publisher1, mut subscriber1) = channel_pair(10);
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut publishing = r#pub::<Channel<i32>>(ready_r, done_s, msg_r);
    let mut publishing = pin!(publishing);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(publishing.as_mut().poll(&mut cx).is_pending());
    ready_s.try_send(publisher0).unwrap();
    ready_s.try_send(publisher1).unwrap();
    let (promise, future) = qpromise();
    msg_s.try_send((426, promise)).unwrap();
    msg_s.close();
    assert!(publishing.as_mut().poll(&mut cx).is_ready());
    assert!(pin!(future.wait()).poll(&mut cx).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
    assert!(done_r.try_recv().unwrap().is_none());
    assert_eq!(
        pin!(subscriber0.next()).poll(&mut cx),
        Poll::Ready(Some(Ok(426)))
    );
    assert_eq!(
        pin!(subscriber1.next()).poll(&mut cx),
        Poll::Ready(Some(Ok(426)))
    );
}

fn pub_two_seed(seed: u64) {
    let mut runner = Runner::new(seed);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    let (publisher0, subscriber0) = runner.channel();
    let (publisher1, subscriber1) = runner.channel();
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut publishing = pin!(r#pub::<Local<i32>>(ready_r, done_s, msg_r));
    ready_s.try_send(publisher0).unwrap();
    ready_s.try_send(publisher1).unwrap();
    let (promise, future) = qpromise();
    msg_s.try_send((426, promise)).unwrap();
    msg_s.close();
    for _ in 0..100 {
        if publishing.as_mut().poll(&mut cx).is_ready() {
            break;
        }
        assert!(runner.step())
    }
    assert!(pin!(future.wait()).poll(&mut cx).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
    assert!(done_r.try_recv().unwrap().is_none());
    assert_eq!(subscriber0.recv(), Some(426));
    assert_eq!(subscriber1.recv(), Some(426));
}

#[test]
fn pub_two_fuzz() {
    for i in 0..1000 {
        pub_two_seed(i);
    }
}
