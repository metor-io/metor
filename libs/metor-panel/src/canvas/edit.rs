//! Gestures, as edits to the source.
//!
//! Every function here takes the program's text and hands back the program's
//! text. That is the whole sync story: a gesture is not a change to the canvas
//! that must later be written down, it is a change to the file that the canvas
//! is redrawn from. Nothing here mutates a model, and nothing outside here
//! knows what the syntax looks like.
//!
//! The edits are deliberately narrow. Connecting an edge rewrites one binding;
//! renaming rewrites one name and the places that name it; adding inserts one
//! declaration; deleting removes one. None of them reformats, reorders, or
//! touches a body — the canvas edits signatures and decorators, never
//! arithmetic, which is what keeps the round trip small enough to trust.
//!
//! ## Renaming is a migration, not a substitution
//!
//! A declaration's name is its card title, its output frame's name, the prefix
//! of every component it publishes, and the key its state is restored by. So a
//! rename has to carry all of those or the feature feels haunted: the state
//! key follows the system name, which `metor_expr::state` keys on, and the
//! consumers that named the old declaration have to name the new one. What a
//! rename must *not* do is touch an unrelated occurrence of the same word,
//! which is why this works from the compiler's spans rather than from a search
//! and replace.

use metor_expr::{Binding, Decl, Manifest, Span};

/// Where a declaration's name sits in the source.
///
/// The compiler reports each declaration's whole region; the name is the
/// identifier inside it, and finding it is a small scan rather than a second
/// parse. Both forms are unambiguous: a `def` names itself after the keyword,
/// and a binding names itself before the `=`.
fn name_span(source: &str, span: Span, name: &str) -> Option<Span> {
    let region = source.get(span.start as usize..span.end as usize)?;
    let at = match region.find("def ") {
        Some(def) => region[def + 4..].find(name)? + def + 4,
        None => region.find(name)?,
    };
    let start = span.start + at as u32;
    Some(Span {
        start,
        end: start + name.len() as u32,
    })
}

/// The span of one declaration's name, whichever list it lives in.
fn declaration_name(manifest: &Manifest, source: &str, decl: Decl) -> Option<(String, Span)> {
    match decl {
        Decl::System(i) => {
            let system = &manifest.systems[i];
            Some((
                system.name.clone(),
                name_span(source, system.source, &system.name)?,
            ))
        }
        Decl::Stage(i) => {
            let stage = &manifest.stages[i];
            Some((
                stage.name.clone(),
                name_span(source, stage.source_span, &stage.name)?,
            ))
        }
    }
}

/// Rename one declaration and everything that names it.
///
/// The edits are collected first and applied last-to-first, so an earlier
/// replacement cannot move a later one's span out from under it.
pub fn rename(manifest: &Manifest, source: &str, decl: Decl, to: &str) -> Option<String> {
    let (from, span) = declaration_name(manifest, source, decl)?;
    if to == from || !is_identifier(to) {
        return None;
    }
    if manifest.systems.iter().any(|s| s.name == to) || manifest.stages.iter().any(|s| s.name == to)
    {
        return None;
    }

    let mut edits = vec![span];
    // Every consumer that reads this declaration reads it by name, so each of
    // those references moves too.
    for other in manifest.declarations() {
        if other == decl {
            continue;
        }
        let (region, bindings): (Span, Vec<&Binding>) = match other {
            Decl::System(i) => (
                manifest.systems[i].source,
                manifest.systems[i]
                    .inputs
                    .iter()
                    .map(|p| &p.bindings[0])
                    .collect(),
            ),
            Decl::Stage(i) => (
                manifest.stages[i].source_span,
                vec![&manifest.stages[i].source],
            ),
        };
        if bindings.iter().any(|b| names(b, decl)) {
            edits.extend(occurrences(source, region, &from));
        }
    }

    edits.sort_by_key(|s| s.start);
    edits.dedup_by_key(|s| s.start);
    let mut out = source.to_string();
    for span in edits.into_iter().rev() {
        out.replace_range(span.start as usize..span.end as usize, to);
    }
    Some(out)
}

/// Whether a binding reads this declaration.
fn names(binding: &Binding, decl: Decl) -> bool {
    match (binding, decl) {
        (Binding::Produced { system, .. }, Decl::System(i)) => *system == i,
        (Binding::Resampled { stage }, Decl::Stage(i)) => *stage == i,
        _ => false,
    }
}

/// Whole-word occurrences of a name inside one declaration's region.
fn occurrences(source: &str, region: Span, name: &str) -> Vec<Span> {
    let Some(text) = source.get(region.start as usize..region.end as usize) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(hit) = text[at..].find(name) {
        let start = at + hit;
        let end = start + name.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let boundary = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary(before) && boundary(after) {
            found.push(Span {
                start: region.start + start as u32,
                end: region.start + end as u32,
            });
        }
        at = end;
    }
    found
}

/// Point one port of one declaration at a different producer.
///
/// This is what connecting an edge means: a port is already bound to
/// something, and the gesture says bind it to this instead. Only the
/// consumer's text changes — an edge has no existence apart from the two names
/// at its ends.
pub fn connect(
    manifest: &Manifest,
    source: &str,
    consumer: Decl,
    port: usize,
    producer: &str,
) -> Option<String> {
    let (region, binding) = match consumer {
        Decl::System(i) => {
            let system = &manifest.systems[i];
            (system.source, &system.inputs.get(port)?.bindings[0])
        }
        Decl::Stage(i) => (manifest.stages[i].source_span, &manifest.stages[i].source),
    };

    // What the manifest records is the *resolved* binding, and what the source
    // says may be the bare name a suffix search resolved. Both spellings are
    // tried, longest first, so `wheels.rpm` is preferred over `rpm` where the
    // text has it in full.
    let at = written_as(manifest, binding)
        .iter()
        .find_map(|text| occurrences(source, region, text).into_iter().next())?;
    let mut out = source.to_string();
    out.replace_range(at.start as usize..at.end as usize, producer);
    Some(out)
}

/// The spellings a binding may appear as in the source, longest first.
fn written_as(manifest: &Manifest, binding: &Binding) -> Vec<String> {
    match binding {
        Binding::Component(path) => match path.rsplit_once('.') {
            Some((_, leaf)) => vec![path.clone(), leaf.to_string()],
            None => vec![path.clone()],
        },
        Binding::Produced { system, .. } => vec![manifest.systems[*system].name.clone()],
        Binding::Resampled { stage } => vec![manifest.stages[*stage].name.clone()],
    }
}

/// Remove one declaration, and nothing else.
///
/// What used to read it becomes an unbound input, which is an ordinary
/// diagnostic on the affected card rather than a cascade of deletions — the
/// operator asked to delete one thing.
pub fn delete(manifest: &Manifest, source: &str, decl: Decl) -> Option<String> {
    let region = match decl {
        Decl::System(i) => manifest.systems[i].source,
        Decl::Stage(i) => manifest.stages[i].source_span,
    };
    // A declaration's own annotation goes with it, or the file keeps a
    // position for a card that no longer exists.
    let layout = match decl {
        Decl::System(i) => manifest.systems[i].layout,
        Decl::Stage(i) => manifest.stages[i].layout,
    };
    let from = match layout.position.is_some() {
        true => region.start.min(layout.span.start),
        false => region.start,
    };
    let start = line_start(source, from);
    let end = line_end(source, region.end.max(layout.span.end));
    let mut out = source.to_string();
    out.replace_range(start as usize..end as usize, "");
    Some(out)
}

/// Append a declaration to the source, named so it does not collide.
///
/// A palette entry is a line of Python, which is the point: what the palette
/// inserts is exactly what an operator would have typed, so there is nothing
/// the canvas can make that the text cannot.
pub fn insert(manifest: &Manifest, source: &str, stem: &str, body: &str) -> (String, String) {
    let name = fresh(manifest, stem);
    let declaration = body.replace("{name}", &name);
    let mut out = source.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&declaration);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    (out, name)
}

/// A name nothing in the program has taken.
fn fresh(manifest: &Manifest, stem: &str) -> String {
    let taken = |name: &str| {
        manifest.systems.iter().any(|s| s.name == name)
            || manifest.stages.iter().any(|s| s.name == name)
    };
    if !taken(stem) {
        return stem.to_string();
    }
    (2..)
        .map(|n| format!("{stem}{n}"))
        .find(|n| !taken(n))
        .expect("some suffix is free")
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn line_start(source: &str, at: u32) -> u32 {
    let at = (at as usize).min(source.len());
    source[..at].rfind('\n').map_or(0, |i| i + 1) as u32
}

fn line_end(source: &str, at: u32) -> u32 {
    let at = (at as usize).min(source.len());
    source[at..].find('\n').map_or(source.len(), |i| at + i + 1) as u32
}

#[cfg(test)]
mod tests;
