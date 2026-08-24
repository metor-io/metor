//! What the canvas can add, derived from what the language has.
//!
//! `OpDescriptor::ALL` was a hand-maintained table: twenty-one entries that
//! had to be kept in step with twenty-one op constructors, their inspector
//! rows, and their validators. There is nothing to maintain here, because a
//! palette entry is a line of Python and the language already says which lines
//! exist — the prelude's functions, plus whatever the module itself defines.
//!
//! Entries come in two kinds, and the split is not cosmetic: a **source**
//! needs nothing to exist, so it can always be added, while a **transform**
//! reads something, so it is offered only when a card is selected to read.
//! That way every insertion produces a declaration that is already wired and
//! already compiles — there is no half-built node state for the canvas to
//! hold, which is the same reason the text is the truth.

use metor_expr::Manifest;

/// One thing the palette can add.
pub struct Entry {
    pub label: String,
    pub detail: &'static str,
    /// Python with `{name}` for the declaration's name and `{input}` for the
    /// selected card, if it takes one.
    pub template: String,
    /// The stem a fresh name is built from.
    pub stem: &'static str,
}

/// Sources: a declaration that needs no input, so it is always offerable.
///
/// Each is a self-clocked system, because a generator with nothing to wait on
/// is exactly what `@system(rate=)` is for.
fn sources() -> Vec<Entry> {
    let wave = |label: &str, call: &str| Entry {
        label: label.to_string(),
        detail: "source · 100 Hz",
        template: format!(
            "@system(rate=100.0)\ndef {{name}}() -> f64:\n    return {call}\n"
        ),
        stem: "signal",
    };
    vec![
        wave("sine", "sine(1.0, 1.0)"),
        wave("cosine", "cosine(1.0, 1.0)"),
        wave("square", "square(1.0, 1.0)"),
        wave("sawtooth", "sawtooth(1.0, 1.0)"),
        wave("random", "random()"),
        wave("constant", "constant(1.0)"),
    ]
}

/// Transforms: a declaration that reads one thing, offered against a
/// selection so what it inserts is already connected.
fn transforms(manifest: Option<&Manifest>) -> Vec<Entry> {
    let unary = |label: &'static str, call: &str| Entry {
        label: label.to_string(),
        detail: "transform",
        template: format!("{{name}} = {call}\n"),
        stem: label,
    };
    let mut all = vec![
        unary("scaled", "{input} * 2.0"),
        unary("offset", "{input} + 1.0"),
        unary("sqrt", "sqrt({input})"),
        unary("abs", "abs({input})"),
        unary("log", "log({input})"),
        unary("exp", "exp({input})"),
        unary("floor", "floor({input})"),
        Entry {
            label: "window".to_string(),
            detail: "the last 64 samples",
            template: "{name} = window({input}, 64)\n".to_string(),
            stem: "window",
        },
        Entry {
            label: "fft".to_string(),
            detail: "one-sided magnitudes",
            template: "{name} = fft(window({input}, 64))\n".to_string(),
            stem: "spectrum",
        },
        Entry {
            label: "resample (hold)".to_string(),
            detail: "onto a 10 Hz clock",
            template: "{name} = resample_zoh({input}, 10.0)\n".to_string(),
            stem: "held",
        },
        Entry {
            label: "resample (linear)".to_string(),
            detail: "onto a 10 Hz clock",
            template: "{name} = resample_linear({input}, 10.0)\n".to_string(),
            stem: "smooth",
        },
    ];

    // Whatever the module itself defines is offerable too, on the same terms
    // as anything in the prelude — which is what makes the palette derived
    // rather than declared.
    for sig in manifest.map(|m| m.functions.as_slice()).unwrap_or_default() {
        if sig.params.len() != 1 {
            continue;
        }
        all.push(Entry {
            label: sig.name.clone(),
            detail: "defined in this module",
            template: format!("{{name}} = {}({{input}})\n", sig.name),
            stem: "derived",
        });
    }
    all
}

/// Everything offerable right now.
///
/// With nothing selected the transforms are absent rather than disabled: an
/// entry that cannot be completed is not an entry.
pub fn entries(manifest: Option<&Manifest>, selected: Option<&str>) -> Vec<Entry> {
    let mut all = sources();
    if let Some(input) = selected {
        for mut entry in transforms(manifest) {
            entry.template = entry.template.replace("{input}", input);
            all.push(entry);
        }
    }
    all
}

#[cfg(test)]
mod tests;
