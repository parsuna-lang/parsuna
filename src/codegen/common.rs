//! Case conversion and string escaping helpers shared by every backend.

use crate::grammar::ir::*;
use crate::lowering::{StateTable, TokenInfo};

/// Human-readable form of a token for user-facing messages.
///
/// For tokens whose resolved pattern is a single string literal, the
/// literal in backticks (e.g. `` `>` `` for the `GT` token whose
/// pattern is `">"`); otherwise the grammar-declared `name`. Already
/// debug-escaped (`\\`, `\"`, control codes are pre-applied), so the
/// result drops safely into any backend's `"..."` string literal —
/// every supported language uses the same backslash-escape vocabulary.
///
/// Lives here rather than on `TokenInfo` so the IR carries only the
/// raw `name` + `pattern`; rendering is a codegen / dump concern.
pub fn token_display_name(t: &TokenInfo) -> String {
    match &t.pattern {
        TokenPattern::Literal(s) => {
            // {:?} on &str gives `"..."` with `\\`, `\"`, `\n`, etc.
            // already applied; strip the outer quotes and re-wrap in
            // backticks for the user-visible form.
            let escaped = format!("{:?}", s);
            let inner = escaped
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(escaped.as_str());
            format!("`{}`", inner)
        }
        _ => t.name.clone(),
    }
}

/// Look up a kind id's display name in `st.tokens`. Returns `"EOF"`
/// for kind 0 and `"?"` for unknown kinds; otherwise routes through
/// [`token_display_name`].
pub fn token_display_name_for_kind(st: &StateTable, kind: u16) -> String {
    if kind == 0 {
        return "EOF".to_string();
    }
    match st.tokens.iter().find(|t| t.kind == kind) {
        Some(t) => token_display_name(t),
        None => "?".to_string(),
    }
}

/// Compose the user-facing `errorHere` argument for a list of token
/// kinds: ``expected `{` | `[` | STRING | NUMBER | `true` | `false` | `null` ``
/// for a multi-kind list, ``expected `>` `` for a singleton.
pub fn expected_message(st: &StateTable, kinds: &[u16]) -> String {
    let names: Vec<String> = kinds
        .iter()
        .map(|k| token_display_name_for_kind(st, *k))
        .collect();
    format!("expected {}", names.join(" | "))
}

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
        Expr::Opt(x) | Expr::Star(x) | Expr::Plus(x) | Expr::Label(_, x) => visit(x, f),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_capitalizes_words_and_drops_underscores() {
        assert_eq!(pascal("hello_world"), "HelloWorld");
        assert_eq!(pascal("foo"), "Foo");
        assert_eq!(pascal("ABC_DEF"), "AbcDef");
        assert_eq!(pascal(""), "");
        assert_eq!(pascal("_leading"), "Leading");
        assert_eq!(pascal("a_b_c"), "ABC");
    }

    #[test]
    fn screaming_snake_uppercases_and_inserts_underscores() {
        assert_eq!(screaming_snake("helloWorld"), "HELLO_WORLD");
        assert_eq!(screaming_snake("foo"), "FOO");
        assert_eq!(screaming_snake("FooBar"), "FOO_BAR");
        assert_eq!(screaming_snake("already_snake"), "ALREADY_SNAKE");
    }

    #[test]
    fn escape_string_handles_special_chars() {
        assert_eq!(escape_string("hi"), "hi");
        assert_eq!(escape_string("a\"b"), "a\\\"b");
        assert_eq!(escape_string("a\\b"), "a\\\\b");
        assert_eq!(escape_string("\n"), "\\n");
        assert_eq!(escape_string("\r"), "\\r");
        assert_eq!(escape_string("\t"), "\\t");
    }

    #[test]
    fn escape_string_uses_braced_unicode_for_low_controls() {
        // Below 0x20 → `\u{0001}` style.
        assert_eq!(escape_string("\x01"), "\\u{0001}");
    }

    #[test]
    fn escape_string_bmp_uses_unbraced_unicode_for_low_controls() {
        // BMP variant (Java/C# style) → ``.
        assert_eq!(escape_string_bmp("\x01"), "\\u0001");
    }

    #[test]
    fn visit_hits_every_subexpression_pre_order() {
        // (A B)? — visits Opt, Seq, A, B
        let e = Expr::Opt(Box::new(Expr::Seq(vec![
            Expr::Token("A".into()),
            Expr::Token("B".into()),
        ])));
        let mut seen: Vec<String> = Vec::new();
        visit(&e, &mut |x| {
            seen.push(match x {
                Expr::Empty => "Empty".into(),
                Expr::Token(n) => format!("Token({})", n),
                Expr::Rule(n) => format!("Rule({})", n),
                Expr::Seq(_) => "Seq".into(),
                Expr::Alt(_) => "Alt".into(),
                Expr::Opt(_) => "Opt".into(),
                Expr::Star(_) => "Star".into(),
                Expr::Plus(_) => "Plus".into(),
                Expr::Label(name, _) => format!("Label({})", name),
            });
        });
        assert_eq!(seen, vec!["Opt", "Seq", "Token(A)", "Token(B)"]);
    }
}

