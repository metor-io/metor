# Docs pass 2: kill noise comments, sharpen first sentences

Pass 1 (committed as f51af4b0) rewrote every doc/comment in the house
style. This pass applies three refinements from review of that output,
now codified in `.claude/skills/rewrite-docs/SKILL.md`:

1. **Re-exports get no comment.** Readers know what a `pub use` is;
   delete commentary on re-export blocks.
2. **Trait impls get no comment.** `impl Trait for Type` speaks for
   itself. A genuinely non-obvious contract moves to the trait or
   module doc instead.
3. **First sentence says what the thing is.** Every type/trait doc must
   open with a concrete statement a newcomer can parse ("A frame groups
   components into one contiguous block of memory"), never a
   restatement of the name.
4. **Cross-link freely.** Use intra-doc links (`[Frame]`,
   [`crate::coordinator::Coordinator`]) to related items.

Scope: same 59 .rs files as pass 1 (metor-fsw-2 src + macros + ring +
tests + fixtures), one agent per file, code token-identical throughout.
Follow-ups also in scope at the end: fixture/ring `Cargo.toml`
descriptions and `ring/MIRI.md` still carry pass-1-era jargon;
`dynamic.rs` kept `docs/frames.md §` references the other files dropped.

Verify after: `cargo fmt --check`, `cargo test --all-features`,
`cargo clippy --all-features` on all three crates, then commit.
