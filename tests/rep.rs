use std::{
    pin::pin,
    task::{Context, Poll},
};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, SinkExt, StreamExt};
use stream_patterns::rep;

mod common;

#[test]
fn rep_one() {
    let (replier, mut requester) = channel_pair(10);
    let (ready_s, ready_r) = bounded(10);
    let (done_s, done_r) = bounded(10);
    let (request_s, request_r) = bounded(10);
    let mut replying = rep::<Channel<i32>>(ready_r, done_s, request_s);
    let mut replying = pin!(replying);
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(replying.as_mut().poll(&mut cx).is_pending());
    ready_s.try_send(replier).unwrap();
    assert!(pin!(requester.send(426)).poll(&mut cx).is_ready());
    assert!(replying.as_mut().poll(&mut cx).is_pending());
    let (request, promise) = request_r.try_recv().unwrap();
    promise.resolve(request - 210);
    ready_s.close();
    assert!(replying.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        pin!(requester.next()).poll(&mut cx),
        Poll::Ready(Some(Ok(216)))
    );
    drop(requester);
    assert!(replying.as_mut().poll(&mut cx).is_ready());
    assert!(done_r.try_recv().unwrap().is_none());
}
