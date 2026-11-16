//! The configuration editor's form logic, independent of how it is drawn.
//!
//! Works on the tree of [`crate::conf_edit`] with the directive
//! descriptions of [`crate::conf_schema`]: where a block sits, which
//! directives may be added to it, how a directive's arguments map to the
//! fields of its form and back, and what the server says about the text.
//! A block is addressed by its path: the index of each enclosing block
//! from the top of the file (`[]` is the top level).

use crate::conf_edit::{Item, quote};
use crate::conf_schema::{Arg, Kind, Spec};
use serde::Serialize;

// ------------------------------------------------------------ contexts

/// The context inside block `name` written in context `ctx`: `server` in
/// `rtmp` is `server`, in `http` it is `http_server`. `other` for blocks
/// this server does not know.
pub fn inner_ctx(name: &str, ctx: &str) -> &'static str {
    match (ctx, name) {
        ("main", "rtmp") => "rtmp",
        ("main", "http") => "http",
        ("rtmp", "server") => "server",
        ("server", "application") => "application",
        ("application", "recorder") => "recorder",
        ("http", "server") => "http_server",
        ("http_server", "location") => "location",
        _ => "other",
    }
}

/// How a directive this server does not know is shown, by where it is.
pub struct Unknown {
    /// Short label for the badge.
    pub label: &'static str,
    pub help: &'static str,
    /// Worth a warning: probably a typo or a feature not ported yet.
    pub warn: bool,
}

/// Outside `rtmp {}` an unknown directive is an nginx one (`worker_processes`,
/// `events {}`, most of `http {}`) that has no effect here; inside, it is
/// probably misspelled or not implemented.
pub fn unknown(ctx: &str) -> Unknown {
    if matches!(ctx, "rtmp" | "server" | "application" | "recorder") {
        Unknown {
            label: "not supported",
            help: "This server ignores this directive. Check the spelling, or it may not be implemented yet.",
            warn: true,
        }
    } else {
        Unknown {
            label: "nginx only",
            help: "Not used by this server. Kept so the file also works with nginx.",
            warn: false,
        }
    }
}

/// How a context is named on screen.
pub fn ctx_label(ctx: &str) -> &str {
    match ctx {
        "main" => "Main",
        "server" => "RTMP server",
        "application" => "Application",
        "recorder" => "Recorder",
        "http_server" => "HTTP server",
        "location" => "Location",
        "other" => "Block",
        c => c,
    }
}

/// The description of directive `name` in context `ctx`.
pub fn spec_for<'a>(specs: &'a [Spec], name: &str, ctx: &str) -> Option<&'a Spec> {
    specs.iter().find(|s| s.name == name && s.ctx.contains(&ctx))
}

// ---------------------------------------------------------------- paths

/// The block at `path`, if `path` names one.
pub fn item_at<'a>(tree: &'a [Item], path: &[usize]) -> Option<&'a Item> {
    let (mut items, mut it) = (tree, None);
    for &i in path {
        let item = items.get(i)?;
        items = block_of(item)?;
        it = Some(item);
    }
    it
}

/// The contents of the block at `path` (the whole file for `[]`).
pub fn items_at<'a>(tree: &'a [Item], path: &[usize]) -> Option<&'a [Item]> {
    let mut items = tree;
    for &i in path {
        items = block_of(items.get(i)?)?;
    }
    Some(items)
}

pub fn items_at_mut<'a>(tree: &'a mut Vec<Item>, path: &[usize]) -> Option<&'a mut Vec<Item>> {
    let mut items = tree;
    for &i in path {
        match items.get_mut(i)? {
            Item::Dir { block: Some(b), .. } => items = b,
            _ => return None,
        }
    }
    Some(items)
}

/// The context of the block at `path`.
pub fn ctx_of(tree: &[Item], path: &[usize]) -> &'static str {
    let (mut ctx, mut items) = ("main", tree);
    for &i in path {
        let Some(Item::Dir { name, block: Some(b), .. }) = items.get(i) else { return "other" };
        ctx = inner_ctx(name, ctx);
        items = b;
    }
    ctx
}

/// `path` if it still names a block (after the tree changed), else the
/// top level.
pub fn valid_path(tree: &[Item], path: &[usize]) -> Vec<usize> {
    if path.is_empty() || item_at(tree, path).is_some() { path.to_vec() } else { Vec::new() }
}

fn block_of(item: &Item) -> Option<&[Item]> {
    match item {
        Item::Dir { block: Some(b), .. } => Some(b),
        _ => None,
    }
}

// --------------------------------------------------------- block names

/// Name and detail of a block, as shown in the tree: `server` blocks show
/// their `listen` addresses (or the default port).
pub fn block_title(item: &Item, ctx: &str) -> (String, String) {
    let Item::Dir { name, args, block, .. } = item else { return (String::new(), String::new()) };
    let inner = inner_ctx(name, ctx);
    if inner == "server" || inner == "http_server" {
        let listen: Vec<String> = block
            .iter()
            .flatten()
            .filter_map(|d| match d {
                Item::Dir { name, args, .. } if name == "listen" => Some(args.join(" ")),
                _ => None,
            })
            .collect();
        let detail = if !listen.is_empty() {
            listen.join(", ")
        } else if inner == "server" {
            "1935".into()
        } else {
            "80".into()
        };
        return (name.clone(), detail);
    }
    (name.clone(), args.join(" "))
}

/// A block in the tree on the left.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub path: Vec<usize>,
    /// 1 for blocks at the top level.
    pub depth: usize,
    pub name: String,
    pub detail: String,
}

/// Every block of the file, in order, depth first.
pub fn blocks(tree: &[Item]) -> Vec<Node> {
    fn walk(items: &[Item], path: &mut Vec<usize>, ctx: &str, out: &mut Vec<Node>) {
        for (i, it) in items.iter().enumerate() {
            let Item::Dir { name, block: Some(b), .. } = it else { continue };
            path.push(i);
            let (name_, detail) = block_title(it, ctx);
            out.push(Node { path: path.clone(), depth: path.len(), name: name_, detail });
            walk(b, path, inner_ctx(name, ctx), out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(tree, &mut Vec::new(), "main", &mut out);
    out
}

/// `nginx.conf › rtmp › server 1935 › application live`.
pub fn crumbs(tree: &[Item], path: &[usize]) -> Vec<String> {
    let mut out = vec!["nginx.conf".to_string()];
    for k in 1..=path.len() {
        if let Some(it) = item_at(tree, &path[..k]) {
            let (n, d) = block_title(it, ctx_of(tree, &path[..k - 1]));
            out.push(format!("{n} {d}").trim().to_string());
        }
    }
    out
}

/// Number of directives directly inside a block.
pub fn directive_count(items: &[Item]) -> usize {
    items.iter().filter(|i| matches!(i, Item::Dir { .. })).count()
}

// ------------------------------------------------------------ arguments

/// Split typed text into arguments, honouring quotes like the parser.
pub fn split_args(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < c.len() {
        if c[i].is_whitespace() {
            i += 1;
        } else if c[i] == '"' || c[i] == '\'' {
            let q = c[i];
            i += 1;
            let mut w = String::new();
            while i < c.len() && c[i] != q {
                if c[i] == '\\' && i + 1 < c.len() {
                    i += 1;
                }
                w.push(c[i]);
                i += 1;
            }
            i += 1;
            out.push(w);
        } else {
            let mut w = String::new();
            while i < c.len() && !c[i].is_whitespace() {
                w.push(c[i]);
                i += 1;
            }
            out.push(w);
        }
    }
    out
}

/// Arguments as text, quoted where needed.
pub fn join_args<S: AsRef<str>>(args: &[S]) -> String {
    args.iter().map(|a| quote(a.as_ref())).collect::<Vec<_>>().join(" ")
}

/// The value of one field of a directive's form.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    One(String),
    /// Repeated arguments, `choices` and `param` fields, or arguments left
    /// over after the last field.
    Many(Vec<String>),
}

impl Val {
    pub fn as_list(&self) -> Vec<String> {
        match self {
            Val::One(s) => vec![s.clone()],
            Val::Many(v) => v.clone(),
        }
    }
}

/// Spread a directive's arguments over the fields of its description.
pub fn assign(spec: &Spec, args: &[String]) -> Vec<Val> {
    let required = |from: usize| spec.args[from..].iter().filter(|a| !a.opt).count();
    let (mut out, mut k) = (Vec::with_capacity(spec.args.len()), 0usize);
    for (n, a) in spec.args.iter().enumerate() {
        if a.rep {
            out.push(Val::Many(args[k.min(args.len())..].to_vec()));
            k = args.len();
            continue;
        }
        if matches!(a.kind, Kind::Choices(_)) {
            let mut vals = Vec::new();
            while k < args.len() && args.len() - k > required(n + 1) {
                vals.push(args[k].clone());
                k += 1;
            }
            out.push(Val::Many(vals));
            continue;
        }
        let left = args.len() - k;
        if a.opt {
            let fits = left > required(n + 1)
                && match a.kind {
                    Kind::Choice(values) => values.contains(&args[k].as_str()),
                    _ => true,
                };
            if fits {
                out.push(Val::One(args[k].clone()));
                k += 1;
            } else {
                out.push(Val::One(String::new()));
            }
            continue;
        }
        if k < args.len() {
            out.push(Val::One(args[k].clone()));
            k += 1;
        } else {
            out.push(Val::One(String::new()));
        }
    }
    if k < args.len() {
        match out.last_mut() {
            Some(last) => {
                let mut v = last.as_list();
                v.extend_from_slice(&args[k..]);
                *last = Val::Many(v);
            }
            None => out.push(Val::Many(args[k..].to_vec())),
        }
    }
    out
}

/// A directive's arguments from the fields of its form.
pub fn rebuild(spec: &Spec, vals: &[Val]) -> Vec<String> {
    let mut args = Vec::new();
    for (n, v) in vals.iter().enumerate() {
        let opt = spec.args.get(n).is_none_or(|a| a.opt);
        match v {
            Val::Many(list) => args.extend(list.iter().filter(|x| !x.is_empty()).cloned()),
            Val::One(s) if !s.is_empty() || !opt => args.push(s.clone()),
            Val::One(_) => {}
        }
    }
    args
}

/// Change a directive's arguments; its original text no longer applies.
pub fn set_args(item: &mut Item, new: Vec<String>) {
    if let Item::Dir { args, raw, .. } = item {
        *args = new;
        *raw = None;
    }
}

/// `key=value` → (`key`, `value`).
pub fn split_param(kv: &str) -> (&str, &str) {
    kv.split_once('=').unwrap_or((kv, ""))
}

/// Whether `s` is a valid value for a field of kind `kind` (empty fields
/// are not checked here).
pub fn valid(kind: &Kind, s: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match kind {
        Kind::Int => digits(s),
        Kind::Size => {
            let n = s.strip_suffix(['k', 'K', 'm', 'M', 'g', 'G']).unwrap_or(s);
            digits(n)
        }
        Kind::Time => valid_time(s),
        Kind::Addr => {
            if digits(s) {
                return true;
            }
            if let Some(rest) = s.strip_prefix('[') {
                return rest.split_once("]:").is_some_and(|(ip, port)| {
                    !ip.is_empty() && ip.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.') && digits(port)
                });
            }
            s.split_once(':').is_some_and(|(host, port)| {
                !host.is_empty() && !host.chars().any(|c| c.is_whitespace()) && digits(port)
            })
        }
        _ => true,
    }
}

/// nginx time: one or more `<digits>[ms|s|m|h|d|w|M|y]`.
fn valid_time(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.is_empty() {
        return false;
    }
    while i < b.len() {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
        if b[i..].starts_with(b"ms") {
            i += 2;
        } else if i < b.len() && b"smhdwMy".contains(&b[i]) {
            i += 1;
        }
    }
    true
}

/// Hint shown in an empty field.
pub fn placeholder(arg: &Arg) -> &str {
    match arg.kind {
        Kind::Time => "e.g. 30s",
        Kind::Size => "e.g. 4096 or 1m",
        Kind::Int => "number",
        Kind::Addr => "port or ip:port",
        Kind::Url => "url",
        Kind::Path => "path",
        _ => arg.name,
    }
}

// ------------------------------------------------------------- changes

/// Directives that may be added to a block in context `ctx` holding
/// `items`, grouped by module in the order of the descriptions. Those
/// written once at most are left out when already present.
pub fn available<'a>(specs: &'a [Spec], items: &[Item], ctx: &str) -> Vec<(&'static str, Vec<&'a Spec>)> {
    let present = |n: &str| items.iter().any(|i| matches!(i, Item::Dir { name, .. } if name == n));
    let mut groups: Vec<(&'static str, Vec<&Spec>)> = Vec::new();
    for s in specs.iter().filter(|s| s.ctx.contains(&ctx) && (s.multi || !present(s.name))) {
        match groups.iter_mut().find(|(m, _)| *m == s.module) {
            Some((_, list)) => list.push(s),
            None => groups.push((s.module, vec![s])),
        }
    }
    groups
}

pub fn dir(name: &str, args: Vec<String>, block: Option<Vec<Item>>) -> Item {
    Item::Dir { name: name.into(), args, block, comment: None, raw: None, end_comment: None, line: 0 }
}

/// A new directive for `spec`, with a value in each required field.
pub fn new_directive(spec: &Spec) -> Item {
    let mut args = Vec::new();
    for a in spec.args.iter().filter(|a| !a.opt) {
        args.push(match a.kind {
            Kind::Flag => (if spec.default == "on" { "off" } else { "on" }).to_string(),
            Kind::Choice(v) | Kind::Choices(v) => {
                (if v.contains(&spec.default) { spec.default } else { v.first().copied().unwrap_or("") }).to_string()
            }
            Kind::Addr => spec.default.to_string(),
            _ => String::new(),
        });
    }
    dir(spec.name, args, spec.block.map(|_| Vec::new()))
}

/// Add a new `spec` directive to `items`: after the last one of the same
/// name, else at the end. Returns its index, and whether it is a block
/// that needs nothing else (so the editor can open it).
pub fn add(items: &mut Vec<Item>, spec: &Spec) -> (usize, bool) {
    let at = items
        .iter()
        .rposition(|i| matches!(i, Item::Dir { name, .. } if name == spec.name))
        .map_or(items.len(), |k| k + 1);
    items.insert(at, new_directive(spec));
    (at, spec.block.is_some() && spec.args.iter().all(|a| a.opt))
}

/// `name args` typed by hand (a trailing `;` is allowed).
pub fn parse_custom(text: &str) -> Option<Item> {
    let t = text.trim();
    let mut w = split_args(t.strip_suffix(';').unwrap_or(t));
    if w.is_empty() || w[0].is_empty() {
        return None;
    }
    let name = w.remove(0);
    Some(dir(&name, w, None))
}

/// Where item `i` goes when moved one place up (`-1`) or down (`1`):
/// past the next item that is not a blank line (blank lines are not
/// shown by the editor).
fn move_target(items: &[Item], i: usize, delta: isize) -> Option<usize> {
    if i >= items.len() {
        return None;
    }
    let mut j = i as isize;
    loop {
        j += delta;
        let k = usize::try_from(j).ok().filter(|&k| k < items.len())?;
        if !matches!(items[k], Item::Blank) {
            return Some(k);
        }
    }
}

/// Whether item `i` can move by `delta` (see [`move_item`]).
pub fn can_move(items: &[Item], i: usize, delta: isize) -> bool {
    move_target(items, i, delta).is_some()
}

/// Move item `i` one place up (`-1`) or down (`1`), over blank lines.
pub fn move_item(items: &mut Vec<Item>, i: usize, delta: isize) -> bool {
    let Some(j) = move_target(items, i, delta) else { return false };
    let it = items.remove(i);
    items.insert(j, it);
    true
}

/// A simple directive turned into a comment (`# name args;`).
pub fn disable(item: &Item) -> Option<Item> {
    let Item::Dir { name, args, block: None, .. } = item else { return None };
    let mut words = vec![name.clone()];
    words.extend(args.iter().cloned());
    Some(Item::Comment { text: format!(" {};", join_args(&words)) })
}

/// Whether a comment holds a directive that was commented out.
pub fn is_disabled_directive(text: &str) -> bool {
    let t = text.trim_start();
    let b = t.as_bytes();
    if b.is_empty() || !(b[0].is_ascii_lowercase() || b[0] == b'_') {
        return false;
    }
    let end = b.iter().position(|&c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')).unwrap_or(b.len());
    if b.get(end).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        return false;
    }
    t.trim_end().ends_with(';')
}

/// The directive in a comment made by [`disable`].
pub fn enable(text: &str) -> Option<Item> {
    if !is_disabled_directive(text) {
        return None;
    }
    parse_custom(text.trim_end())
}

// ------------------------------------------------------------- checking

/// What the server says about a configuration text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

impl Check {
    /// Check `text` as the file `file_name`.
    pub fn of(text: &str, file_name: &str) -> Check {
        match crate::config::Config::parse(text, file_name) {
            Ok(c) => Check { ok: true, error: None, warnings: c.warnings },
            Err(e) => Check { ok: false, error: Some(e.to_string()), warnings: Vec::new() },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Err,
}

/// The status lines for a check: its error and warnings, or that the
/// configuration is valid.
pub fn check_messages(c: &Check, dirty: bool) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    if let Some(e) = &c.error {
        out.push((Level::Err, e.clone()));
    }
    out.extend(c.warnings.iter().map(|w| (Level::Warn, w.clone())));
    if c.ok && c.warnings.is_empty() {
        let m = if dirty { "Configuration is valid (not saved yet)." } else { "Configuration is valid." };
        out.push((Level::Ok, m.into()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf_edit::parse;
    use crate::conf_schema::specs;

    const CONF: &str = "\
worker_processes 1;
rtmp {
    server {
        listen 1935;
        listen 1936;
        application live {
            live on;
            # record all;
            recorder rec {
                record video;
            }
        }
    }
    server {
        application vod { play /video; }
    }
}
http {
    server {
        location /stat { rtmp_stat all; }
    }
}
";

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn spec(name: &str, ctx: &str) -> Spec {
        specs().into_iter().find(|s| s.name == name && s.ctx.contains(&ctx)).expect(name)
    }

    #[test]
    fn contexts_and_paths() {
        let tree = parse(CONF).unwrap();
        assert_eq!(ctx_of(&tree, &[]), "main");
        assert_eq!(ctx_of(&tree, &[1]), "rtmp");
        assert_eq!(ctx_of(&tree, &[1, 0]), "server");
        assert_eq!(ctx_of(&tree, &[1, 0, 2]), "application");
        assert_eq!(ctx_of(&tree, &[1, 0, 2, 2]), "recorder");
        assert_eq!(ctx_of(&tree, &[2, 0]), "http_server");
        assert_eq!(ctx_of(&tree, &[2, 0, 0]), "location");
        assert!(item_at(&tree, &[0]).is_none(), "a simple directive is not a block");
        assert!(item_at(&tree, &[9]).is_none());
        assert_eq!(items_at(&tree, &[1, 0]).unwrap().len(), 3);
        assert_eq!(valid_path(&tree, &[1, 0, 2]), vec![1, 0, 2]);
        assert_eq!(valid_path(&tree, &[1, 7]), Vec::<usize>::new());
        let mut t = tree.clone();
        items_at_mut(&mut t, &[1, 1]).unwrap().clear();
        assert_eq!(directive_count(items_at(&t, &[1, 1]).unwrap()), 0);
    }

    #[test]
    fn tree_nodes_and_titles() {
        let tree = parse(CONF).unwrap();
        let nodes: Vec<(Vec<usize>, usize, String, String)> =
            blocks(&tree).into_iter().map(|n| (n.path, n.depth, n.name, n.detail)).collect();
        let want: Vec<(Vec<usize>, usize, String, String)> = [
            (vec![1], 1, "rtmp", ""),
            (vec![1, 0], 2, "server", "1935, 1936"),
            (vec![1, 0, 2], 3, "application", "live"),
            (vec![1, 0, 2, 2], 4, "recorder", "rec"),
            (vec![1, 1], 2, "server", "1935"),
            (vec![1, 1, 0], 3, "application", "vod"),
            (vec![2], 1, "http", ""),
            (vec![2, 0], 2, "server", "80"),
            (vec![2, 0, 0], 3, "location", "/stat"),
        ]
        .into_iter()
        .map(|(p, d, n, x)| (p, d, n.to_string(), x.to_string()))
        .collect();
        assert_eq!(nodes, want);
        assert_eq!(crumbs(&tree, &[1, 0, 2]), s(&["nginx.conf", "rtmp", "server 1935, 1936", "application live"]));
    }

    #[test]
    fn split_and_join() {
        assert_eq!(split_args(r#"a "b c" 'd' "e\"f""#), s(&["a", "b c", "d", "e\"f"]));
        assert_eq!(split_args("  "), Vec::<String>::new());
        assert_eq!(join_args(&s(&["a", "b c", ""])), r#"a "b c" """#);
        assert_eq!(split_args(&join_args(&s(&["x y", "q\"z"]))), s(&["x y", "q\"z"]));
    }

    #[test]
    fn assign_and_rebuild() {
        let sp = spec("live", "application");
        assert_eq!(assign(&sp, &s(&["on"])), vec![Val::One("on".into())]);
        // too many arguments go to the last field
        assert_eq!(assign(&sp, &s(&["on", "extra"])), vec![Val::Many(s(&["on", "extra"]))]);
        // required + repeated
        let sp = spec("exec", "application");
        let v = assign(&sp, &s(&["ffmpeg", "-i", "x"]));
        assert_eq!(v, vec![Val::One("ffmpeg".into()), Val::Many(s(&["-i", "x"]))]);
        assert_eq!(rebuild(&sp, &v), s(&["ffmpeg", "-i", "x"]));
        // a missing required argument stays empty and is still written
        let v = assign(&sp, &[]);
        assert_eq!(v, vec![Val::One(String::new()), Val::Many(vec![])]);
        assert_eq!(rebuild(&sp, &v), s(&[""]));
        // address + repeated options
        let sp = spec("listen", "server");
        assert_eq!(assign(&sp, &s(&["1935", "ipv6only=on"])), vec![Val::One("1935".into()), Val::Many(s(&["ipv6only=on"]))]);
        // choices that are not repeated
        let sp = spec("record", "recorder");
        assert_eq!(assign(&sp, &s(&["audio", "keyframes"])), vec![Val::Many(s(&["audio", "keyframes"]))]);
    }

    #[test]
    fn every_directive_round_trips() {
        for sp in specs() {
            let args: Vec<String> = sp
                .args
                .iter()
                .filter(|a| !a.opt && !a.rep)
                .map(|a| match a.kind {
                    Kind::Choice(v) | Kind::Choices(v) => v[0].to_string(),
                    _ => "x".into(),
                })
                .collect();
            let vals = assign(&sp, &args);
            assert_eq!(vals.len(), sp.args.len(), "{}", sp.name);
            assert_eq!(rebuild(&sp, &vals), args, "{} round trip", sp.name);
        }
    }

    #[test]
    fn validation() {
        for ok in ["30s", "500ms", "1m30s", "1h", "10", "2d", "1y"] {
            assert!(valid(&Kind::Time, ok), "{ok}");
        }
        for bad in ["s", "1x", "1 s", "ms", ""] {
            assert!(!valid(&Kind::Time, bad), "{bad}");
        }
        assert!(valid(&Kind::Size, "4096") && valid(&Kind::Size, "8k") && valid(&Kind::Size, "1M"));
        assert!(!valid(&Kind::Size, "1kb") && !valid(&Kind::Size, "k"));
        assert!(valid(&Kind::Int, "12") && !valid(&Kind::Int, "-1"));
        for ok in ["1935", "0.0.0.0:1935", "*:1935", "[::1]:1935", "host:80"] {
            assert!(valid(&Kind::Addr, ok), "{ok}");
        }
        for bad in ["host", ":80", "[::1]", "a b:80", "1935:"] {
            assert!(!valid(&Kind::Addr, bad), "{bad}");
        }
        assert!(valid(&Kind::Text, "anything at all"));
    }

    #[test]
    fn adding() {
        let all = specs();
        let tree = parse(CONF).unwrap();
        let app = items_at(&tree, &[1, 0, 2]).unwrap();
        let names: Vec<&str> =
            available(&all, app, "application").iter().flat_map(|(_, l)| l.iter().map(|s| s.name)).collect();
        assert!(!names.contains(&"live"), "written once and already present");
        assert!(names.contains(&"exec") && names.contains(&"recorder"));
        assert!(!names.contains(&"listen"), "not valid in an application");
        let mods: Vec<&str> = available(&all, &[], "application").iter().map(|(m, _)| *m).collect();
        let mut unique = mods.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(mods.len(), unique.len(), "one group per module");

        let mut items = parse("listen 1935;\nchunk_size 4096;\n").unwrap();
        assert_eq!(add(&mut items, &spec("listen", "server")), (1, false), "after the other listen");
        assert!(matches!(&items[1], Item::Dir { name, args, .. } if name == "listen" && args == &s(&["1935"])));
        assert_eq!(add(&mut items, &spec("application", "server")), (3, false), "needs its name first");
        assert!(matches!(&items[3], Item::Dir { block: Some(b), args, .. } if b.is_empty() && args == &s(&[""])));
        assert!(add(&mut items, &spec("rtmp", "main")).1);
        // a flag starts as the opposite of its default
        assert!(matches!(new_directive(&spec("live", "application")), Item::Dir { args, .. } if args == s(&["on"])));
    }

    #[test]
    fn custom_moving_and_comments() {
        assert!(matches!(parse_custom(" foo bar \"b z\"; "), Some(Item::Dir { name, args, .. }) if name == "foo" && args == s(&["bar", "b z"])));
        assert!(parse_custom("  ;").is_none());

        let mut items = parse("a 1;\nb 2;\nc 3;\n").unwrap();
        assert!(move_item(&mut items, 0, 1));
        assert!(!move_item(&mut items, 0, -1));
        assert!(!move_item(&mut items, 2, 1));
        assert!(matches!(&items[0], Item::Dir { name, .. } if name == "b"));
        let mut d = items[1].clone();
        // blank lines are passed over and stay where they are
        let mut blanks = parse("a 1;\n\nb 2;\n").unwrap();
        assert_eq!(blanks.len(), 3);
        assert!(can_move(&blanks, 0, 1) && !can_move(&blanks, 0, -1) && !can_move(&blanks, 2, 1));
        assert!(move_item(&mut blanks, 0, 1));
        assert!(matches!(&blanks[1], Item::Dir { name, .. } if name == "b"));
        assert!(matches!(&blanks[2], Item::Dir { name, .. } if name == "a"));
        assert!(matches!(&blanks[0], Item::Blank));

        set_args(&mut d, s(&["x y"]));
        assert!(matches!(&d, Item::Dir { raw: None, .. }));
        let c = disable(&d).unwrap();
        assert_eq!(c, Item::Comment { text: " a \"x y\";".into() });
        let Item::Comment { text } = &c else { unreachable!() };
        assert!(is_disabled_directive(text));
        assert!(matches!(enable(text), Some(Item::Dir { name, args, .. }) if name == "a" && args == s(&["x y"])));
        for not in [" just a note", " Live on;", " live on", " 9x on;", " liveX on;"] {
            assert!(!is_disabled_directive(not), "{not}");
        }
        assert!(is_disabled_directive(" record all; "));
        assert!(disable(&parse("rtmp { }").unwrap()[0]).is_none(), "blocks are not commented out");
    }

    #[test]
    fn checking() {
        let ok = Check::of("rtmp { server { listen 1935; } }", "t.conf");
        assert!(ok.ok && ok.error.is_none());
        assert_eq!(check_messages(&ok, true), vec![(Level::Ok, "Configuration is valid (not saved yet).".to_string())]);
        let bad = Check::of("rtmp { server { listen; } }", "t.conf");
        assert!(!bad.ok);
        assert_eq!(check_messages(&bad, false)[0].0, Level::Err);
        assert_eq!(serde_json::to_value(&bad).unwrap()["ok"], false);
    }

    #[test]
    fn unknown_by_place() {
        assert!(!unknown("main").warn);
        assert!(!unknown("other").warn);
        assert!(!unknown("location").warn);
        assert!(unknown("application").warn);
        assert!(unknown("rtmp").warn);
    }
}
