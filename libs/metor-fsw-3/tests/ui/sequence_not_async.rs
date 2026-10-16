use metor_fsw_3::{Seq, SeqError, sequence};

#[sequence]
fn arm(seq: &Seq) -> Result<(), SeqError> {
    let _ = seq;
    Ok(())
}

fn main() {}
