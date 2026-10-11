//! Finds Rust items compiled only for tests: `#[cfg(test)]`,
//! `#[cfg(all(test, ..))]`, and test functions (`#[test]`, `#[tokio::test]`).
//!
//! This is a small lexer, not a parser: it skips comments, strings and char
//! literals, tracks bracket depth, and ends an annotated item at its closing
//! brace, at a `;` or `,` on the same depth, or when the enclosing block ends.

/// Lines (0-based, inclusive) of test-only items, or `None` when the whole
/// file is test-only (`#![cfg(test)]` at the top level).
pub fn test_regions(src: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut lexer = Lexer {
        src,
        pos: 0,
        line: 0,
    };
    let mut regions = Vec::new();
    let mut depth = 0usize;
    // Attribute being read: (start line, depth before `[`, inner?, text start).
    let mut attr: Option<(usize, usize, bool, usize)> = None;
    let mut item: Option<Item> = None;

    while let Some(token) = lexer.next_code() {
        let c = match token {
            Token::Punct(c) => c,
            Token::Ident(word) => {
                if let Some(item) = item.as_mut().filter(|i| depth == i.depth && !i.braced) {
                    item.keyword |= ITEM_KEYWORDS.contains(&word);
                }
                continue;
            }
        };
        match c {
            b'#' if attr.is_none() => {
                let start = lexer.line;
                let inner = lexer.eat_ws_then(b'!');
                if lexer.eat_ws_then(b'[') {
                    attr = Some((start, depth, inner, lexer.pos));
                    depth += 1;
                }
            }
            b'<' | b'>' => {
                let arrow = lexer.pos >= 2 && matches!(src[lexer.pos - 2], b'-' | b'=');
                if let Some(item) = item.as_mut().filter(|i| depth == i.depth && !arrow) {
                    item.angles += if c == b'<' { 1 } else { -1 };
                }
            }
            b'{' | b'(' | b'[' => {
                if let Some(item) = item.as_mut().filter(|i| c == b'{' && depth == i.depth) {
                    item.braced = true;
                }
                depth += 1;
            }
            b'}' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                if let Some((start, attr_depth, inner, text)) = attr
                    && c == b']'
                    && depth == attr_depth
                {
                    attr = None;
                    if item.is_none() && is_test_attr(&src[text..lexer.pos - 1]) {
                        if inner {
                            if depth == 0 {
                                return None;
                            }
                        } else {
                            item = Some(Item {
                                start,
                                depth,
                                braced: false,
                                keyword: false,
                                angles: 0,
                            });
                        }
                    }
                } else if let Some(open) = &item
                    && (depth < open.depth || (c == b'}' && depth == open.depth && open.braced))
                {
                    regions.push((open.start, lexer.line));
                    item = None;
                }
            }
            b';' | b',' => {
                if let Some(open) = &item
                    && depth == open.depth
                    && !open.braced
                    && (c == b';' || (!open.keyword && open.angles <= 0))
                {
                    regions.push((open.start, lexer.line));
                    item = None;
                }
            }
            _ => {}
        }
    }
    if let Some(open) = item {
        regions.push((open.start, lexer.line));
    }
    // Doc comments and attributes stacked above `#[cfg(test)]` belong to the item.
    let lines: Vec<&[u8]> = src.split(|&b| b == b'\n').collect();
    for (start, _) in &mut regions {
        while *start > 0 {
            let above = lines[*start - 1].trim_ascii_start();
            if !(above.starts_with(b"#[") || above.starts_with(b"///")) {
                break;
            }
            *start -= 1;
        }
    }
    Some(regions)
}

/// Keywords after which only a closing brace or `;` ends an item, so commas in
/// generics or where clauses do not.
const ITEM_KEYWORDS: &[&[u8]] = &[
    b"fn",
    b"impl",
    b"trait",
    b"struct",
    b"enum",
    b"union",
    b"mod",
    b"where",
    b"macro_rules",
];

struct Item {
    start: usize,
    depth: usize,
    braced: bool,
    /// An item keyword was seen: `,` no longer ends the item.
    keyword: bool,
    angles: i32,
}

enum Token<'a> {
    Punct(u8),
    Ident(&'a [u8]),
}

fn is_test_attr(attr: &[u8]) -> bool {
    let text: Vec<u8> = attr
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let name = text.split(|&b| b == b'(').next().unwrap_or_default();
    if text == b"cfg(test)" || name == b"test" || name.ends_with(b"::test") {
        return true;
    }
    let Some(args) = text
        .strip_prefix(b"cfg(all(")
        .and_then(|t| t.strip_suffix(b"))"))
    else {
        return false;
    };
    // Only top-level `test` arguments of all(..) count; nested any(..) does not.
    let mut level = 0i32;
    let mut start = 0;
    for (i, &b) in args.iter().chain(b",").enumerate() {
        match b {
            b'(' => level += 1,
            b')' => level -= 1,
            b',' if level == 0 => {
                if &args[start..i] == b"test" {
                    return true;
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    false
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn peek(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = *self.src.get(self.pos)?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn eat_ws_then(&mut self, want: u8) -> bool {
        let (pos, line) = (self.pos, self.line);
        while self.peek(0).is_some_and(|c| c.is_ascii_whitespace()) {
            self.bump();
        }
        if self.peek(0) == Some(want) {
            self.bump();
            true
        } else {
            (self.pos, self.line) = (pos, line);
            false
        }
    }

    /// Returns the next identifier or punctuation outside comments and literals.
    fn next_code(&mut self) -> Option<Token<'a>> {
        loop {
            let c = self.bump()?;
            match c {
                b'/' if self.peek(0) == Some(b'/') => {
                    while self.peek(0).is_some_and(|c| c != b'\n') {
                        self.bump();
                    }
                }
                b'/' if self.peek(0) == Some(b'*') => {
                    self.bump();
                    let mut nesting = 1;
                    while nesting > 0 {
                        match self.bump()? {
                            b'/' if self.peek(0) == Some(b'*') => {
                                self.bump();
                                nesting += 1;
                            }
                            b'*' if self.peek(0) == Some(b'/') => {
                                self.bump();
                                nesting -= 1;
                            }
                            _ => {}
                        }
                    }
                }
                b'"' => self.skip_string(),
                b'\'' => self.skip_char_or_lifetime(),
                c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                    let start = self.pos - 1;
                    while self
                        .peek(0)
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
                    {
                        self.bump();
                    }
                    let word = &self.src[start..self.pos];
                    if matches!(word, b"r" | b"br" | b"cr") && self.skip_raw_string() {
                        continue;
                    }
                    return Some(Token::Ident(word));
                }
                c if c.is_ascii_punctuation() => return Some(Token::Punct(c)),
                _ => {}
            }
        }
    }

    fn skip_string(&mut self) {
        while let Some(c) = self.bump() {
            match c {
                b'\\' => {
                    self.bump();
                }
                b'"' => return,
                _ => {}
            }
        }
    }

    /// After `r`, `br` or `cr`: skips `#*"..."#*` if present (not `r#ident`).
    fn skip_raw_string(&mut self) -> bool {
        let hashes = (0..).take_while(|&i| self.peek(i) == Some(b'#')).count();
        if self.peek(hashes) != Some(b'"') {
            return false;
        }
        for _ in 0..=hashes {
            self.bump();
        }
        while let Some(c) = self.bump() {
            if c == b'"' && (0..hashes).all(|i| self.peek(i) == Some(b'#')) {
                for _ in 0..hashes {
                    self.bump();
                }
                return true;
            }
        }
        true
    }

    fn skip_char_or_lifetime(&mut self) {
        if self.peek(0) == Some(b'\\') {
            self.bump();
            self.bump();
            while self.peek(0).is_some_and(|c| c != b'\'' && c != b'\n') {
                self.bump();
            }
            self.bump();
            return;
        }
        // A char literal holds one (possibly multi-byte) char before the quote.
        let width = match self.peek(0) {
            Some(c) if c < 0x80 => 1,
            Some(c) if c >= 0xF0 => 4,
            Some(c) if c >= 0xE0 => 3,
            Some(_) => 2,
            None => return,
        };
        if self.peek(width) == Some(b'\'') {
            for _ in 0..=width {
                self.bump();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_test_items_only() {
        let src = r####"fn prod() { let s = "#[cfg(test)] mod fake {"; let c = '{'; }
/* #[cfg(test)]
   mod nope { } */
#[cfg(test)]
mod tests {
    fn t<'a>(x: &'a str) { let r = r#"}"#; let q = '}'; }
}
fn after() {}
#[cfg(test)] use std::fmt;
#[cfg(any(test, feature = "x"))]
fn not_test_only() {}
/// Test helper.
#[cfg(all(feature = "x", test))]
fn helper<T, U>(x: T) -> Vec<u8>
where
    T: Into<String>,
{
    vec![]
}
struct S {
    a: u8,
    #[cfg(test)]
    b: Vec<u8>,
    c: u8,
}
#[tokio::test(flavor = "multi_thread")]
async fn loose() {}
"####;
        assert_eq!(
            test_regions(src.as_bytes()),
            Some(vec![(3, 6), (8, 8), (11, 18), (21, 22), (25, 26)])
        );
        assert_eq!(test_regions(b"//! doc\n#![cfg(test)]\nfn x() {}\n"), None);
    }
}
