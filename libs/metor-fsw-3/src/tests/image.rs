//! A frame holding a 1024² image.

use metor_proto::com_de::{AsComponentView, Decomponentize};
use metor_proto::types::ComponentId;

use super::utils::ImageBuf;

#[test]
fn test_apply_value_in_place_on_small_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut source = ImageBuf::default();
            let source = source.stamped(7);
            let mut target = ImageBuf::default();
            let target = target.stamped(0);
            let applied = target.apply_value(
                ComponentId::new("pixels"),
                source.pixels.as_component_view(),
                None,
            );
            assert!(applied.is_ok());
            assert_eq!(target.pixels.pixels(), source.pixels.pixels());
        })
        .expect("spawn");
    assert!(handle.join().is_ok());
}
