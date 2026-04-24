//! Case conversion and string escaping helpers shared by every backend.

use crate::grammar::ir::*;

/// Visit every sub-expression of `e` (pre-order). Used by backends that
/// need to inspect every referenced token/rule in a rule body.
pub fn visit<F: FnMut(&Expr)>(e: &Expr, f: &mut F) {
    f(e);
    match e {
        Expr::Empty | Expr::Token(_) | Expr::Rule(_) => {}
        Expr::Seq(xs) | Expr::Alt(xs) => {
            for x in xs {
                visit(x, f);
            }
        }
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) => visit(x, f),
    }
}

/// Convert `snake_case` (or similar) to `PascalCase`. Underscores mark
/// word boundaries and are dropped; every following character is
/// capitalised.
pub fn pascal(s: &str) -> String {
    let mut out = String::new();
    let mut upper_next = true;
    for c in s.chars() {
        if c == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

pub fn screaming_snake(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c);
            prev_lower = false;
        } else if c == '_' {
            out.push('_');
            prev_lower = false;
        } else {
            out.extend(c.to_uppercase());
            prev_lower = c.is_ascii_lowercase();
        }
    }
    out
}

pub fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn escape_string_bmp(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

