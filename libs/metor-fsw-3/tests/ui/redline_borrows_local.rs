use metor_fsw_3::{Redline, Seq, SeqError, sequence};

#[sequence]
async fn dangle(seq: &Seq) -> Result<(), SeqError> {
    let value = Box::new(42u32);
    let redline = Box::pin(Redline::new("dangling", || *value == 42));
    seq.redline(redline.as_ref());
    core::mem::forget(redline);
    drop(value);
    seq.next_cycle().await;
    Ok(())
}

fn main() {}
