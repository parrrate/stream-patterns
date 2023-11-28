use async_channel::{Receiver, Sender};

use crate::{promise::QPromise, sub, Done, PatternStream};

pub async fn pull<S: PatternStream>(
    ready_r: Receiver<S>,
    done_s: Sender<Done<S>>,
    msg_s: Sender<(S::Msg, QPromise)>,
) {
    sub(ready_r, done_s, msg_s).await
}
