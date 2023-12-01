use std::{pin::pin, task::Context};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, SinkExt};
use fuzzer::{Local, Runner};
use stream_patterns::pull;

mod common;

#[test]
fn pull_one() {
    let (puller, mut pusher) = channel_pair(10);
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut pulling = pull::<Channel<i32>>(ready_r, done_s, msg_s);
    let mut pulling = pin!(pulling);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(pulling.as_mut().poll(&mut cx).is_pending());
    ready_s.try_send(puller).unwrap();
    assert!(pin!(pusher.send(426)).poll(&mut cx).is_ready());
    assert!(pulling.as_mut().poll(&mut cx).is_pending());
    let (msg, promise) = msg_r.try_recv().unwrap();
    assert_eq!(msg, 426);
    promise.done();
    ready_s.close();
    drop(pusher);
    assert!(pulling.as_mut().poll(&mut cx).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
}

fn pull_one_seed(seed: u64) {
    let mut runner = Runner::new(seed);
    let (puller, pusher) = runner.channel();
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (msg_s, msg_r) = bounded(10);
    let mut pulling = pin!(pull::<Local<i32>>(ready_r, done_s, msg_s));
    ready_s.try_send(puller).unwrap();
    pusher.send(426);
    runner.pending_in(100, pulling.as_mut());
    let (msg, promise) = msg_r.try_recv().unwrap();
    assert_eq!(msg, 426);
    promise.done();
    ready_s.close();
    drop(pusher);
    runner.ready_in(100, pulling.as_mut());
    assert!(done_r.try_recv().unwrap().is_none());
}

#[test]
fn pull_one_fuzz() {
    for i in 0..1000 {
        pull_one_seed(i);
    }
}
