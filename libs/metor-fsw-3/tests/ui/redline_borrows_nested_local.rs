use core::future::{Future, poll_fn};
use core::task::Poll;
use metor_fsw_3::{Seq, SeqError, redline, sequence};

#[sequence]
async fn dangle(seq: &Seq<'_>) -> Result<(), SeqError> {
    let mut nested = Box::pin(async {
        let value = Box::new(42u32);
        redline!(seq, "dangling", || *value == 42);
        seq.next_cycle().await;
    });
    poll_fn(|cx| {
        let _ = nested.as_mut().poll(cx);
        Poll::Ready(())
    })
    .await;
    core::mem::forget(nested);
    seq.next_cycle().await;
    Ok(())
}

fn main() {}
