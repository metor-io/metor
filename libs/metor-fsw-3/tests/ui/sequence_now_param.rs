use metor_fsw_3::{Seq, SeqError, Timestamp, sequence};

#[sequence]
async fn arm(seq: &Seq, now: Timestamp) -> Result<(), SeqError> {
    let _ = (seq, now);
    Ok(())
}

fn main() {}
