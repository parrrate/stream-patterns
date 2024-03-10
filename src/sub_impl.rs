use async_channel::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use ruchei::{close_all::CloseAllExt, multicast::ignore::MulticastIgnore};

use crate::{
    promise::{QPromise, RequestSender},
    Done, DoneCallback, PatternStream,
};

pub async fn sub<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_s: Sender<(S::Msg, QPromise)>,
) {
    let mut stream = ready_r
        .close_all()
        .multicast_ignore(DoneCallback::new(done_s));
    while let Some(msg) = stream.next().await {
        let msg = msg.unwrap_or_else(|e| match e {});
        if msg_s.request(msg).await.is_err() {
            break;
        }
    }
    stream.close().await.unwrap_or_else(|e| match e {});
}
