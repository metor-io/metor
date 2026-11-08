//! A frame holding a 1024² image.

use metor_proto::com_de::{AsComponentView, FromComponentView};

use super::utils::ImageBuf;

#[test]
fn test_assign_in_place_on_small_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut source = ImageBuf::default();
            let source = source.stamped(7);
            let mut target = ImageBuf::default();
            let target = target.stamped(0);
            let applied = target
                .pixels
                .assign_component_view(source.pixels.as_component_view());
            assert!(applied.is_ok());
            assert_eq!(target.pixels.pixels(), source.pixels.pixels());
        })
        .expect("spawn");
    assert!(handle.join().is_ok());
}
