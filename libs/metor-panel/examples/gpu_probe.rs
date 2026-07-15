//! Standalone GPU init check: prints the adapter enumeration and which
//! device tier succeeded without opening the panel UI. First thing to run
//! when plots come up blank on a new machine:
//! `cargo run -p metor-panel --example gpu_probe`
//! (optionally with `WGPU_BACKEND=vulkan|dx12|gl`).

fn main() {
    let _ = tracing_subscriber::fmt::fmt()
        .with_target(false)
        .with_env_filter(tracing_subscriber::EnvFilter::builder().parse_lossy("info"))
        .try_init();
    match metor_panel::gpu_context::GpuContext::get() {
        Some(ctx) => println!("GpuContext OK: {:?}", ctx.adapter.get_info()),
        None => println!("GpuContext init failed"),
    }
}
