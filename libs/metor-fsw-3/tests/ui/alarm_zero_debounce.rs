use metor_fsw_3::AlarmSpec;

const HOT: AlarmSpec = AlarmSpec::warning("hot").debounce(0);

fn main() {
    let _ = HOT;
}
