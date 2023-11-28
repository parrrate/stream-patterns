use std::{
    pin::pin,
    task::{Context, Poll},
};

use async_channel::bounded;
use common::{channel_pair, Channel};
use futures_util::{Future, StreamExt};
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
