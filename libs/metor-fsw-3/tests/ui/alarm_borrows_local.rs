use metor_fsw_3::sequence::SpecName;
use metor_fsw_3::{AlarmSpec, Seq, SeqError, Watch, sequence};

const HOT: AlarmSpec = AlarmSpec::warning("hot");

#[sequence]
async fn dangle(seq: &Seq) -> Result<(), SeqError> {
    let value = Box::new(42u32);
    let alarm = Box::pin(Watch::alarm(&HOT, || *value == 42, SpecName));
    seq.watch(alarm.as_ref());
    core::mem::forget(alarm);
    drop(value);
    seq.next_cycle().await;
    Ok(())
}

fn main() {}
