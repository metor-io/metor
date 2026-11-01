use metor_fsw_3::sequence::SpecName;
use metor_fsw_3::{Seq, SeqError, Watch, sequence};

#[sequence]
async fn dangle(seq: &Seq) -> Result<(), SeqError> {
    let value = Box::new(42u32);
    let redline = Box::pin(Watch::redline("dangling", || *value == 42, SpecName));
    seq.watch(redline.as_ref());
    core::mem::forget(redline);
    drop(value);
    seq.next_cycle().await;
    Ok(())
}

fn main() {}
