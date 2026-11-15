//! `nginx.conf` as an editable document.
//!
//! [`parse`] reads a configuration file into a tree that keeps comments,
//! blank lines and the original text of every simple directive, and
//! [`render`] writes it back. A file parsed and rendered without changes
//! keeps its content (only indentation is normalised to four spaces), so
//! the configuration editor can change one directive without rewriting
//! the rest of the file. The typed configuration ([`crate::config`]) is
//! built from the rendered text, never from this tree.

use serde::{Deserialize, Serialize};

/// One line (or block) of a configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Item {
    /// `name args;` or `name args { ... }`.
    Dir {
        name: String,
        #[serde(default)]
        args: Vec<String>,
        /// Contents of `{ ... }`; `None` for a simple directive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<Vec<Item>>,
        /// Comment written after the directive on the same line (without `#`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
        /// Original text of a simple directive, without the `;`. Used when
        /// rendering as long as it still reads as `name args`, so quoting
        /// and spacing are kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<String>,
        /// Comment after the closing `}` of a block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_comment: Option<String>,
        /// Line in the file it was read from (0 for new directives).
        #[serde(default)]
        line: usize,
    },
    /// `# text` on a line of its own (`text` excludes the `#`).
    Comment { text: String },
    /// One or more empty lines.
    Blank,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Open,
    Close,
    Semi,
    Comment(String),
}

struct Token {
    tok: Tok,
    line: usize,
    /// Newlines between the previous token and this one.
    nl_before: usize,
    start: usize,
}

fn scan(src: &str) -> Result<Vec<Token>, String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut line, mut nl) = (0, 1, 0);
    while i < b.len() {
        let c = b[i];
        let start = i;
        let push = |out: &mut Vec<Token>, tok, nl: &mut usize, line| {
            out.push(Token { tok, line, nl_before: *nl, start });
            *nl = 0;
        };
        match c {
            b'\n' => {
                line += 1;
                nl += 1;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            b'#' => {
                let end = src[i..].find('\n').map_or(b.len(), |n| i + n);
                let text = src[i + 1..end].trim_end_matches('\r').to_string();
                push(&mut out, Tok::Comment(text), &mut nl, line);
                i = end;
            }
            b'{' => {
                push(&mut out, Tok::Open, &mut nl, line);
                i += 1;
            }
            b'}' => {
                push(&mut out, Tok::Close, &mut nl, line);
                i += 1;
            }
            b';' => {
                push(&mut out, Tok::Semi, &mut nl, line);
                i += 1;
            }
            b'"' | b'\'' => {
                let first = line;
                let mut s = String::new();
                let mut chars = src[i + 1..].char_indices();
                let end = loop {
                    match chars.next() {
                        None => return Err(format!("line {first}: unterminated string")),
                        Some((n, ch)) if ch as u32 == c as u32 => break i + 1 + n + 1,
                        Some((_, '\\')) => {
                            if let Some((_, n)) = chars.next() {
                                if n == '\n' {
                                    line += 1;
                                }
                                s.push(n);
                            }
                        }
                        Some((_, ch)) => {
                            if ch == '\n' {
                                line += 1;
                            }
                            s.push(ch);
                        }
                    }
                };
                push(&mut out, Tok::Word(s), &mut nl, first);
                i = end;
            }
            _ => {
                let end = src[i..]
                    .find(|ch: char| ch.is_whitespace() || matches!(ch, '{' | '}' | ';' | '#'))
                    .map_or(b.len(), |n| i + n);
                push(&mut out, Tok::Word(src[i..end].to_string()), &mut nl, line);
                i = end;
            }
        }
    }
    Ok(out)
}

/// Read a configuration file into an editable tree. Errors carry the line.
pub fn parse(src: &str) -> Result<Vec<Item>, String> {
    let toks = scan(src)?;
    let mut pos = 0;
    parse_items(src, &toks, &mut pos, false)
}

fn parse_items(src: &str, toks: &[Token], pos: &mut usize, nested: bool) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = Vec::new();
    loop {
        let Some(t) = toks.get(*pos) else {
            if nested {
                return Err("unexpected end of file, expecting \"}\"".into());
            }
            return Ok(items);
        };
        if t.nl_before >= 2 && !items.is_empty() && !matches!(t.tok, Tok::Close) {
            items.push(Item::Blank);
        }
        match &t.tok {
            Tok::Close if nested => {
                *pos += 1;
                return Ok(items);
            }
            Tok::Comment(text) => {
                items.push(Item::Comment { text: text.clone() });
                *pos += 1;
            }
            Tok::Word(name) => {
                let (name, line, start) = (name.clone(), t.line, t.start);
                *pos += 1;
                let mut args = Vec::new();
                loop {
                    let Some(t) = toks.get(*pos) else {
                        return Err(format!("line {line}: directive \"{name}\" is not terminated by \";\""));
                    };
                    *pos += 1;
                    match &t.tok {
                        Tok::Word(w) => args.push(w.clone()),
                        // A comment in the middle of a directive is dropped.
                        Tok::Comment(_) => {}
                        Tok::Semi => {
                            let raw = src[start..t.start].trim_end();
                            let raw = (!raw.contains('\n')).then(|| raw.to_string());
                            let comment = inline_comment(toks, pos);
                            items.push(Item::Dir { name, args, block: None, comment, raw, end_comment: None, line });
                            break;
                        }
                        Tok::Open => {
                            let comment = inline_comment(toks, pos);
                            let block = parse_items(src, toks, pos, true)?;
                            let end_comment = inline_comment(toks, pos);
                            items.push(Item::Dir { name, args, block: Some(block), comment, raw: None, end_comment, line });
                            break;
                        }
                        Tok::Close => {
                            return Err(format!("line {}: unexpected \"}}\"", t.line));
                        }
                    }
                }
            }
            Tok::Close => return Err(format!("line {}: unexpected \"}}\"", t.line)),
            Tok::Open => return Err(format!("line {}: unexpected \"{{\"", t.line)),
            Tok::Semi => return Err(format!("line {}: unexpected \";\"", t.line)),
        }
    }
}

/// A comment on the same line as the token just read.
fn inline_comment(toks: &[Token], pos: &mut usize) -> Option<String> {
    match toks.get(*pos) {
        Some(Token { tok: Tok::Comment(c), nl_before: 0, .. }) => {
            *pos += 1;
            Some(c.clone())
        }
        _ => None,
    }
}

/// Write the tree back as configuration text.
pub fn render(items: &[Item]) -> String {
    let mut out = String::new();
    write_items(&mut out, items, 0);
    out
}

fn write_items(out: &mut String, items: &[Item], depth: usize) {
    let indent = "    ".repeat(depth);
    let mut prev_blank = true; // no blank line at the start of a block
    for (i, item) in items.iter().enumerate() {
        match item {
            Item::Blank => {
                // nor two in a row, nor one at the end of a block
                if !prev_blank && i + 1 < items.len() {
                    out.push('\n');
                }
                prev_blank = true;
                continue;
            }
            Item::Comment { text } => {
                out.push_str(&indent);
                out.push('#');
                out.push_str(text.trim_end());
                out.push('\n');
            }
            Item::Dir { name, args, block, comment, raw, end_comment, .. } => {
                out.push_str(&indent);
                match block {
                    None => {
                        match raw.as_deref().filter(|r| raw_matches(r, name, args)) {
                            Some(r) => out.push_str(r),
                            None => out.push_str(&directive_text(name, args)),
                        }
                        out.push(';');
                        push_comment(out, comment);
                    }
                    Some(children) => {
                        out.push_str(&directive_text(name, args));
                        out.push_str(" {");
                        push_comment(out, comment);
                        write_items(out, children, depth + 1);
                        out.push_str(&indent);
                        out.push('}');
                        push_comment(out, end_comment);
                    }
                }
            }
        }
        prev_blank = false;
    }
}

fn push_comment(out: &mut String, comment: &Option<String>) {
    if let Some(c) = comment {
        out.push_str(" #");
        out.push_str(c.trim_end());
    }
    out.push('\n');
}

/// `raw` still says `name args` (the editor drops it when it changes a
/// directive, but a stale one must never win).
fn raw_matches(raw: &str, name: &str, args: &[String]) -> bool {
    let Ok(toks) = scan(raw) else { return false };
    let words: Vec<&str> = toks
        .iter()
        .map(|t| match &t.tok {
            Tok::Word(w) => Some(w.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    words.len() == args.len() + 1 && words[0] == name && words[1..].iter().zip(args).all(|(a, b)| a == b)
}

fn directive_text(name: &str, args: &[String]) -> String {
    let mut s = quote(name);
    for a in args {
        s.push(' ');
        s.push_str(&quote(a));
    }
    s
}

/// An argument as configuration text, quoted when it has to be.
pub fn quote(s: &str) -> String {
    let needs = s.is_empty()
        || s.starts_with(['"', '\''])
        || s.chars().any(|c| c.is_whitespace() || matches!(c, '{' | '}' | ';' | '#'));
    if !needs {
        return s.to_string();
    }
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

/// FNV-1a of the file content: lets the editor notice that the file
/// changed on disk since it was loaded.
pub fn content_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "# top comment
worker_processes  1;

events {
    worker_connections  1024; # per worker
}

rtmp {
    server {
        listen 1935;
        # record all manual;

        application live {
            live on;
            push   rtmp://a.example/app   name=x;
            exec ffmpeg -i \"rtmp://localhost/$app/$name\" -c copy out.flv;
        } # end live
    }
}
";

    #[test]
    fn round_trip_keeps_text() {
        let tree = parse(SRC).unwrap();
        assert_eq!(render(&tree), SRC);
        // and the typed parser reads the same thing
        assert!(crate::config::Config::parse(&render(&tree), "t").is_ok());
    }

    #[test]
    fn edits_and_quoting() {
        let mut tree = parse(SRC).unwrap();
        let Item::Dir { block: Some(rtmp), .. } = &mut tree[5] else { panic!("{:?}", tree[5]) };
        let Item::Dir { block: Some(server), .. } = &mut rtmp[0] else { panic!() };
        // stale raw text is ignored
        let Item::Dir { args, .. } = &mut server[0] else { panic!() };
        args[0] = "1936".into();
        server.push(Item::Dir {
            name: "ssl_certificate".into(),
            args: vec!["C:\\certs\\my cert.pem".into()],
            block: None,
            comment: None,
            raw: None,
            end_comment: None,
            line: 0,
        });
        let text = render(&tree);
        assert!(text.contains("        listen 1936;\n"), "{text}");
        assert!(text.contains("ssl_certificate \"C:\\\\certs\\\\my cert.pem\";"), "{text}");
        let again = parse(&text).unwrap();
        assert_eq!(render(&again), text);
        let Item::Dir { block: Some(rtmp), .. } = &again[5] else { panic!() };
        let Item::Dir { block: Some(server), .. } = &rtmp[0] else { panic!() };
        let Some(Item::Dir { args, .. }) = server.last() else { panic!() };
        assert_eq!(args[0], "C:\\certs\\my cert.pem");
    }

    #[test]
    fn errors() {
        assert!(parse("rtmp {").unwrap_err().contains("end of file"));
        assert!(parse("a b").unwrap_err().contains("not terminated"));
        assert!(parse("}").unwrap_err().contains("line 1"));
    }
}
