//! The **sequences** of the `adcs-fsw2` target — the [`commissioning`] ladder and the
//! one-shot [`safe_mode`] drop — as one pack in one `cdylib` (docs/design-packs-authoring.md):
//! plain async fns taking their ports by value, registered by name in [`pack`], and selected
//! by the `mode` slot's `allow occupant=… artifact="seqs"` lines.

mod commissioning;
mod safe_mode;

/// This crate's pack: both sequence occupants, under the names the target's slot
/// `allow`/`initial` lines select.
pub fn pack() -> metor_fsw_2::Pack {
    metor_fsw_2::Pack::new()
        .task("commissioning", commissioning::commissioning)
        .task("safe_mode", safe_mode::safe)
}
metor_fsw_2::export_pack!(pack);
