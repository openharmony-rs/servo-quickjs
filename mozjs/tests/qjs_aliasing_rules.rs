/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Source checks for the re-entrancy rules in HACKING.md: natives, hooks and finalizers that run
//! during a call into QuickJS reach the runtime state and object data again, so the shim must not
//! hold a `&mut` to them across such a call. Neither the compiler nor Miri (the shim runs C code)
//! can check that. Clippy's `mut_from_ref`, denied in `src/engine`, rejects a `&mut` handed out
//! from a shared reference; these tests reject the other patterns that broke it.

use std::path::Path;

/// Functions in `src/engine` that hand out a `&mut` to data behind a raw pointer, with a lifetime
/// no argument bounds. Every caller must end the borrow before calling into QuickJS or the
/// embedder. Add a function here only after checking its callers, and prefer `&T` with cells.
const MUT_ACCESSORS: &[(&str, &str)] = &[
    ("clone.rs", "data_vec"),
    ("clone.rs", "reader"),
    ("clone.rs", "writer"),
    ("module.rs", "module_data"),
    ("module.rs", "module_of"),
    ("module.rs", "request_data"),
];

/// The engine sources, as (path in `src/engine`, lines without `//` comments).
fn engine_sources() -> Vec<(String, Vec<String>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<String>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((name, text.lines().map(strip_comment).collect()));
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine");
    let mut out = vec![];
    walk(&root, &root, &mut out);
    assert!(out.len() > 20, "engine sources not found");
    out
}

/// The line up to a `//` that is not inside a string literal.
fn strip_comment(line: &str) -> String {
    let mut in_string = false;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        if c == '"' && prev != '\\' {
            in_string = !in_string;
        } else if c == '/' && prev == '/' && !in_string {
            return line[..i - 1].to_owned();
        }
        prev = c;
    }
    line.to_owned()
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `text` contains `word` with no identifier characters around it.
fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        !text[..i].ends_with(is_ident) && !text[i + word.len()..].starts_with(is_ident)
    })
}

/// The paths that `line` borrows mutably: `X` for each `&mut X` or `&'a mut X`.
fn mut_borrowed_paths(line: &str) -> Vec<&str> {
    let mut paths = vec![];
    for (i, _) in line.match_indices("mut ") {
        if line[..i].ends_with(is_ident) {
            continue;
        }
        let before = line[..i].trim_end();
        let before = match before.rfind('\'') {
            Some(q) if before[q + 1..].chars().all(is_ident) => before[..q].trim_end(),
            _ => before,
        };
        if before.ends_with('&') {
            paths.push(path_at(line, i + "mut ".len()));
        }
    }
    paths
}

/// The path (`Cx`, `super::Cx`) at byte `i` of `text`, empty if there is none.
fn path_at(text: &str, i: usize) -> &str {
    let rest = &text[i..];
    &rest[..rest
        .find(|c: char| !is_ident(c) && c != ':')
        .unwrap_or(rest.len())]
}

/// Whether `path` names `Cx`.
fn is_cx(path: &str) -> bool {
    path.rsplit("::").next() == Some("Cx")
}

/// The self type of the `impl` block that `line` starts, if it starts one
/// (`impl<'a> Trait for super::Cx {` gives `super::Cx`).
fn impl_self_type(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("impl")?;
    if rest.starts_with(is_ident) {
        return None;
    }
    let mut head = rest.split('{').next()?.trim_start();
    if head.starts_with('<') {
        let mut depth = 0;
        let end = head.char_indices().find_map(|(i, c)| {
            depth += match c {
                '<' => 1,
                '>' => -1,
                _ => 0,
            };
            (depth == 0).then_some(i + 1)
        })?;
        head = &head[end..];
    }
    let head = head.split(" where").next()?;
    Some(head.rsplit(" for ").next()?.trim())
}

#[test]
fn cx_is_only_borrowed_shared() {
    let mut violations = vec![];
    for (file, lines) in engine_sources() {
        let mut in_impl_cx = None;
        for (n, line) in lines.iter().enumerate() {
            let at = format!("src/engine/{file}:{}: {}", n + 1, line.trim());
            let borrowed = mut_borrowed_paths(line);
            if borrowed.iter().any(|p| is_cx(p)) {
                violations.push(format!("{at}\n    a `&mut Cx`"));
            }
            if line.contains("&mut *") && contains_word(line, "Cx") {
                violations.push(format!("{at}\n    a `&mut` made from a `Cx` pointer"));
            }
            // A `&mut Cx` can only be made from a `*mut Cx`: the runtime holds `*const Cx`, and
            // only `JS_DestroyContext` takes the `Box` back.
            let mut_ptr = line
                .match_indices("*mut ")
                .any(|(i, _)| is_cx(path_at(line, i + 5)));
            let destroy =
                file == "runtime.rs" && line.trim_start().starts_with("drop(Box::from_raw(");
            if (mut_ptr || line.contains("NonNull<Cx>")) && !destroy {
                violations.push(format!("{at}\n    a mutable `Cx` pointer"));
            }
            if impl_self_type(line).is_some_and(is_cx) {
                in_impl_cx = Some(line.len() - line.trim_start().len());
            } else if let Some(indent) = in_impl_cx {
                if line.starts_with(&format!("{}}}", " ".repeat(indent))) {
                    in_impl_cx = None;
                } else if borrowed.iter().any(|&p| p == "self" || p == "Self") {
                    violations.push(format!("{at}\n    a `&mut self` method of `Cx`"));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "`Cx` must only be borrowed shared; put state that changes in a cell \
         (see HACKING.md, \"Re-entrancy\"):\n{}",
        violations.join("\n")
    );
}

/// The functions that return `&'a mut` (or `&'static mut`) where no argument mentions `'a`.
fn unbound_mut_accessors() -> Vec<(String, String)> {
    let mut found = vec![];
    for (file, lines) in engine_sources() {
        let text = lines.join("\n");
        for (i, _) in text.match_indices("fn ") {
            if text[..i].ends_with(is_ident) {
                continue;
            }
            let rest = &text[i + 3..];
            let name: String = rest.chars().take_while(|&c| is_ident(c)).collect();
            if name.is_empty() {
                continue;
            }
            let Some(open) = rest.find('(') else { continue };
            let mut depth = 0;
            let mut close = open;
            for (j, c) in rest[open..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    close = open + j;
                    break;
                }
            }
            let args = &rest[open..close];
            let end = rest[close..]
                .find(['{', ';'])
                .map_or(rest.len(), |e| close + e);
            let ret = &rest[close..end];
            let unbound = ret.match_indices("&'").any(|(k, _)| {
                let lifetime: String = ret[k + 1..]
                    .chars()
                    .enumerate()
                    .take_while(|&(m, c)| m == 0 || is_ident(c))
                    .map(|(_, c)| c)
                    .collect();
                ret[k + 1 + lifetime.len()..]
                    .trim_start()
                    .starts_with("mut ")
                    && (lifetime == "'static" || !contains_word(args, &lifetime[1..]))
            });
            if unbound {
                found.push((file.clone(), name));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn mut_accessors_are_listed() {
    let found = unbound_mut_accessors();
    let listed: Vec<(String, String)> = MUT_ACCESSORS
        .iter()
        .map(|&(f, n)| (f.to_owned(), n.to_owned()))
        .collect();
    let new: Vec<_> = found.iter().filter(|f| !listed.contains(f)).collect();
    let gone: Vec<_> = listed.iter().filter(|l| !found.contains(l)).collect();
    assert!(
        new.is_empty(),
        "new functions hand out `&mut` from a raw pointer: return `&T` and put what changes in a \
         cell, or check that no caller keeps the borrow across a call into QuickJS and add them to \
         MUT_ACCESSORS (see HACKING.md, \"Re-entrancy\"): {new:?}"
    );
    assert!(
        gone.is_empty(),
        "no longer hand out `&mut`, remove them from MUT_ACCESSORS: {gone:?}"
    );
}
