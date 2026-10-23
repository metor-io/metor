use metor_fsw_3::{AsVTable, FrameImage, Mono16};

fn main() {
    let _ = FrameImage::<Mono16<17>, 4, 4>::as_vtable();
}
