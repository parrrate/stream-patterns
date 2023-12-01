use std::{
    pin::pin,
    task::{Context, Poll},
};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, SinkExt, StreamExt};
use fuzzer::{Local, Runner};
use stream_patterns::{promise::qpromise, req};

mod common;

#[test]
fn req_one() {
    let (requester, mut replier) = channel_pair(10);
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (request_s, request_r) = bounded(10);
    let mut requesting = req::<Channel<i32>>(ready_r, done_s, request_r);
    let mut requesting = pin!(requesting);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(requesting.as_mut().poll(&mut cx).is_pending());
    ready_s.try_send(requester).unwrap();
    let (promise, future) = qpromise();
    request_s.try_send((426, promise)).unwrap();
    request_s.close();
    assert!(requesting.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        pin!(replier.next()).poll(&mut cx),
        Poll::Ready(Some(Ok(426)))
    );
    assert!(pin!(replier.send(216)).poll(&mut cx).is_ready());
    assert!(requesting.as_mut().poll(&mut cx).is_ready());
    assert_eq!(pin!(future.wait()).poll(&mut cx), Poll::Ready(Ok(216)));
    assert!(done_r.try_recv().is_err());
}

fn req_one_seed(seed: u64) {
    let mut runner = Runner::new(seed);
    let (requester, replier) = runner.channel();
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (request_s, request_r) = bounded(10);
    let mut requesting = pin!(req::<Local<i32>>(ready_r, done_s, request_r));
    ready_s.try_send(requester).unwrap();
    let (promise, future) = qpromise();
    request_s.try_send((426, promise)).unwrap();
    request_s.close();
    runner.pending_in(100, requesting.as_mut());
    assert_eq!(replier.recv(), Some(426));
    replier.send(216);
    runner.ready_in(100, requesting.as_mut());
    assert_eq!(runner.poll(pin!(future.wait())), Poll::Ready(Ok(216)));
    assert!(done_r.try_recv().is_err());
}

#[test]
fn req_one_fuzz() {
    for i in 0..1000 {
        req_one_seed(i);
    }
}
