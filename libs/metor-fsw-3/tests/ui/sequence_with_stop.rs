use metor_fsw_3::{Seq, SeqError, Stop, sequence};

#[sequence]
async fn arm(seq: &Seq, stop: Stop) -> Result<(), SeqError> {
    let _ = (seq, stop);
    Ok(())
}

fn main() {}
