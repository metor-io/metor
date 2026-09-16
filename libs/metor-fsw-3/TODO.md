# TODO

- Restore `#[derive(Record)]` support for serde-compatible enums. Timestamp
  field parsing currently restricts the derive to structs; enum messages
  without timestamps should remain supported. Add a compile-pass regression test.
