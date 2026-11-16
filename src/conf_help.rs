//! Help shown while editing the configuration: what is wrong with a
//! field, the server's messages placed on the directive they are about,
//! settings that need each other, values inherited from outer blocks, and
//! ready-made snippets ("recipes") for common setups.
//!
//! Pure functions over the tree of [`crate::conf_edit`], used by the
//! editor window.

use crate::conf_edit::{self, Item};
use crate::conf_form::{self as form, Check};
use crate::conf_schema::{Arg, Kind, Spec};

/// How much a hint matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Worth knowing.
    Info,
    /// Probably not what was meant.
    Warn,
    /// The server refuses the configuration.
    Err,
}

// --------------------------------------------------------------- fields

/// What is wrong with `value` in field `arg` of `spec`, if anything.
pub fn field_problem(spec: &Spec, arg: &Arg, value: &str) -> Option<&'static str> {
    if value.is_empty() {
        let list = arg.rep || matches!(arg.kind, Kind::Choices(_) | Kind::Param(_));
        return (!arg.opt && !list).then_some("Required.");
    }
    match arg.kind {
        Kind::Int if !form::valid(&arg.kind, value) => Some("Expected a whole number, such as 32."),
        Kind::Size if !form::valid(&arg.kind, value) => Some("Expected bytes, or a size with k, m or g (8k, 1m)."),
        Kind::Time if !form::valid(&arg.kind, value) => Some("Expected a time such as 500ms, 30s, 5m or 1h."),
        Kind::Addr if !form::valid(&arg.kind, value) => Some("Expected a port (1935), ip:port (0.0.0.0:1935) or [ipv6]:port."),
        Kind::Url if spec.module == "notify" && !has_host(value, &["http://"]) => {
            Some("Expected an http://host[:port]/path address.")
        }
        Kind::Url if spec.module == "relay" && !has_host(value, &["rtmp://", "rtmps://"]) => {
            Some("Expected rtmp://host[:port]/app[/stream] (or rtmps://).")
        }
        _ => None,
    }
}

fn has_host(url: &str, schemes: &[&str]) -> bool {
    let lower = url.to_ascii_lowercase();
    schemes.iter().any(|s| lower.strip_prefix(s).is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/')))
}

// ------------------------------------------------------ located messages

/// The server's messages about the configuration, each placed on the
/// directive (path in `tree`) it names, when it names one.
pub fn locate(check: &Check, file: &str, tree: &[Item]) -> Vec<(Vec<usize>, Tone, String)> {
    // Lines of the rendered text: parse it again, the structure is the same.
    let Ok(lined) = conf_edit::parse(&conf_edit::render(tree)) else { return Vec::new() };
    let mut lines = Vec::new();
    fn walk(items: &[Item], path: &mut Vec<usize>, out: &mut Vec<(usize, Vec<usize>)>) {
        for (i, it) in items.iter().enumerate() {
            if let Item::Dir { line, block, .. } = it {
                path.push(i);
                out.push((*line, path.clone()));
                if let Some(b) = block {
                    walk(b, path, out);
                }
                path.pop();
            }
        }
    }
    walk(&lined, &mut Vec::new(), &mut lines);
    let messages = check.error.iter().map(|e| (Tone::Err, e)).chain(check.warnings.iter().map(|w| (Tone::Warn, w)));
    let mut out = Vec::new();
    for (tone, m) in messages {
        let Some((line, text)) = split_location(m, file) else { continue };
        // shown by the directive's own badge
        if text.contains("is not supported yet") {
            continue;
        }
        if let Some((_, p)) = lines.iter().find(|(l, _)| *l == line) {
            out.push((p.clone(), tone, text.to_string()));
        }
    }
    out
}

/// `<file>:<line>: <text>` → (line, text).
fn split_location<'a>(m: &'a str, file: &str) -> Option<(usize, &'a str)> {
    let rest = m.strip_prefix(file)?.strip_prefix(':')?;
    let (n, text) = rest.split_once(':')?;
    Some((n.parse().ok()?, text.trim()))
}

// ---------------------------------------------------------- inheritance

/// The blocks around `path`, nearest first: (path, context, items).
fn ancestors<'a>(tree: &'a [Item], path: &[usize]) -> Vec<(Vec<usize>, &'static str, &'a [Item])> {
    (0..path.len())
        .rev()
        .filter_map(|k| Some((path[..k].to_vec(), form::ctx_of(tree, &path[..k]), form::items_at(tree, &path[..k])?)))
        .collect()
}

fn find<'a>(items: &'a [Item], name: &str) -> Option<&'a [String]> {
    items.iter().rev().find_map(|i| match i {
        Item::Dir { name: n, args, block: None, .. } if n == name => Some(args.as_slice()),
        _ => None,
    })
}

fn has(items: &[Item], name: &str) -> bool {
    items.iter().any(|i| matches!(i, Item::Dir { name: n, .. } if n == name))
}

/// How a block is named in messages: `server 1935`, `application live`.
fn block_name(tree: &[Item], path: &[usize]) -> String {
    match form::item_at(tree, path) {
        Some(it) => {
            let (n, d) = form::block_title(it, form::ctx_of(tree, &path[..path.len() - 1]));
            format!("{n} {d}").trim().to_string()
        }
        None => "the top level".into(),
    }
}

/// The value of `name` that applies in the block at `path`: its own, or
/// the one of the nearest outer block. With where it was set when it is
/// inherited.
fn effective<'a>(tree: &'a [Item], path: &[usize], name: &str) -> Option<(&'a [String], Option<Vec<usize>>)> {
    if let Some(a) = form::items_at(tree, path).and_then(|items| find(items, name)) {
        return Some((a, None));
    }
    ancestors(tree, path).into_iter().find_map(|(p, _, items)| find(items, name).map(|a| (a, Some(p))))
}

/// A setting written in an outer block that applies in this one.
#[derive(Debug, Clone, PartialEq)]
pub struct Inherited {
    pub name: String,
    pub args: Vec<String>,
    /// The block it is written in, e.g. `server 1935`.
    pub from: String,
}

/// The settings of outer blocks that apply in the block at `path` because
/// it does not set them itself.
pub fn inherited(specs: &[Spec], tree: &[Item], path: &[usize]) -> Vec<Inherited> {
    let ctx = form::ctx_of(tree, path);
    if !matches!(ctx, "server" | "application" | "recorder") {
        return Vec::new();
    }
    let Some(own) = form::items_at(tree, path) else { return Vec::new() };
    let mut out: Vec<Inherited> = Vec::new();
    for (p, actx, items) in ancestors(tree, path) {
        let from = block_name(tree, &p);
        for it in items {
            let Item::Dir { name, args, block: None, .. } = it else { continue };
            let applies = form::spec_for(specs, name, actx).is_some_and(|s| s.ctx.contains(&ctx));
            if !applies || has(own, name) || out.iter().any(|o| &o.name == name && o.from != from) {
                continue;
            }
            out.push(Inherited { name: name.clone(), args: args.clone(), from: from.clone() });
        }
    }
    out
}

/// The value of `name` in the nearest outer block of `path` that sets it,
/// and that block's name: what a directive written here overrides.
pub fn overrides(tree: &[Item], path: &[usize], name: &str) -> Option<(Vec<String>, String)> {
    ancestors(tree, path).into_iter().find_map(|(p, _, items)| find(items, name).map(|a| (a.to_vec(), block_name(tree, &p))))
}

// --------------------------------------------------------------- defaults

/// The arguments of `spec`'s default, when it is a value the form can
/// hold (not a description such as `out_queue / 8`).
pub fn default_args(spec: &Spec) -> Option<Vec<String>> {
    // a repeatable directive (listen, push, ...) has no single value to go back to
    if spec.default.is_empty() || spec.multi {
        return None;
    }
    let args = form::split_args(spec.default);
    let vals = form::assign(spec, &args);
    let ok = spec.args.iter().zip(&vals).all(|(a, v)| match v {
        form::Val::One(s) => match a.kind {
            Kind::Choice(values) => s.is_empty() || values.contains(&s.as_str()),
            Kind::Flag => s.is_empty() || s == "on" || s == "off",
            _ => field_problem(spec, a, s).is_none(),
        },
        form::Val::Many(list) => match a.kind {
            Kind::Choices(values) => list.iter().all(|v| values.contains(&v.as_str())),
            _ => a.rep || matches!(a.kind, Kind::Param(_)),
        },
    });
    (ok && form::rebuild(spec, &vals) == args).then_some(args)
}

// ------------------------------------------------------------------ hints

/// Something to add to the open block that settles a hint.
#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    /// Button text.
    pub label: String,
    pub add: Vec<Item>,
}

/// A note about how the settings of a block fit together.
#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    pub tone: Tone,
    pub text: String,
    pub fix: Option<Fix>,
}

fn hint(tone: Tone, text: impl Into<String>) -> Hint {
    Hint { tone, text: text.into(), fix: None }
}

fn with_fix(mut h: Hint, label: &str, text: &str) -> Hint {
    if let Ok(add) = conf_edit::parse(text) {
        h.fix = Some(Fix { label: label.into(), add });
    }
    h
}

const RECORD_SETTINGS: &[&str] = &[
    "record_path",
    "record_suffix",
    "record_unique",
    "record_append",
    "record_lock",
    "record_notify",
    "record_max_size",
    "record_max_frames",
    "record_interval",
    "on_record_done",
    "exec_record_done",
];

/// Notes about the block at `path`: settings that need another one, or
/// that have no effect as written.
pub fn hints(tree: &[Item], path: &[usize]) -> Vec<Hint> {
    let Some(items) = form::items_at(tree, path) else { return Vec::new() };
    let ctx = form::ctx_of(tree, path);
    let eff = |name: &str| effective(tree, path, name).map(|(a, _)| a.to_vec());
    let on = |name: &str| eff(name).is_some_and(|a| a.first().is_some_and(|v| v == "on"));
    let mut out = Vec::new();
    match ctx {
        "rtmp" if !has(items, "server") => out.push(with_fix(
            hint(Tone::Warn, "There is no server block: nothing accepts RTMP connections."),
            "Add a server",
            "server {\n    listen 1935;\n    application live {\n        live on;\n    }\n}\n",
        )),
        "server" => {
            let ssl = items.iter().any(|i| {
                matches!(i, Item::Dir { name, args, .. } if name == "listen" && args.iter().any(|a| a == "ssl"))
            });
            let (cert, key) = (eff("ssl_certificate").is_some(), eff("ssl_certificate_key").is_some());
            if ssl && !(cert && key) {
                let mut add = String::new();
                if !cert {
                    add.push_str("ssl_certificate conf/cert.pem;\n");
                }
                if !key {
                    add.push_str("ssl_certificate_key conf/key.pem;\n");
                }
                out.push(with_fix(
                    hint(Tone::Err, "listen … ssl needs ssl_certificate and ssl_certificate_key (PEM files)."),
                    "Add them",
                    &add,
                ));
            }
            if !ssl && (has(items, "ssl_certificate") || has(items, "ssl_certificate_key")) {
                out.push(hint(Tone::Info, "The certificate is only used by a listen … ssl address, and there is none."));
            }
            if !has(items, "application") {
                out.push(with_fix(
                    hint(Tone::Info, "No application yet: clients have nowhere to publish or play."),
                    "Add application live",
                    "application live {\n    live on;\n}\n",
                ));
            }
        }
        "application" | "recorder" => {
            let pulls = eff("pull").is_some() || eff("exec_pull").is_some();
            if ctx == "application" && !on("live") && !pulls {
                out.push(with_fix(
                    hint(Tone::Warn, "live is off: what publishers send is not passed to players."),
                    "Add live on",
                    "live on;\n",
                ));
            }
            let record = eff("record").filter(|a| a.iter().any(|v| v != "off"));
            let recorders = items.iter().any(|i| matches!(i, Item::Dir { name, block: Some(_), .. } if name == "recorder"));
            match &record {
                Some(_) if eff("record_path").is_none() => out.push(with_fix(
                    hint(Tone::Info, "record_path is not set: recordings are written to the server's folder."),
                    "Add record_path",
                    "record_path recordings;\n",
                )),
                None if !recorders && RECORD_SETTINGS.iter().any(|n| has(items, n)) => out.push(with_fix(
                    hint(Tone::Info, "record is off here, so the record settings have no effect."),
                    "Add record all",
                    "record all;\n",
                )),
                _ => {}
            }
            let unused = |setting: &str, needs: &[&str], what: &str, out: &mut Vec<Hint>| {
                if has(items, setting) && needs.iter().all(|n| eff(n).is_none()) {
                    out.push(hint(Tone::Info, format!("{setting} has no effect without {what}.")));
                }
            };
            unused("push_reconnect", &["push"], "push", &mut out);
            unused("pull_reconnect", &["pull"], "pull", &mut out);
            unused("relay_buffer", &["pull"], "pull", &mut out);
            unused("notify_update_timeout", &["on_update"], "on_update", &mut out);
            unused("notify_update_strict", &["on_update"], "on_update", &mut out);
            unused("respawn", &["exec_push", "exec", "exec_pull"], "exec_push or exec_pull", &mut out);
            unused("respawn_timeout", &["exec_push", "exec", "exec_pull"], "exec_push or exec_pull", &mut out);
            out.extend(access_order(items));
        }
        "http_server" => {
            let used = items.iter().any(|i| match i {
                Item::Dir { name, block: Some(b), .. } if name == "location" => has(b, "rtmp_stat") || has(b, "rtmp_control"),
                _ => false,
            });
            if !used {
                out.push(with_fix(
                    hint(Tone::Info, "This HTTP server is not started: none of its locations uses rtmp_stat or rtmp_control."),
                    "Add a statistics page",
                    STAT_LOCATIONS,
                ));
            }
        }
        "location" if has(items, "rtmp_stat_stylesheet") && !has(items, "rtmp_stat") => {
            out.push(hint(Tone::Info, "rtmp_stat_stylesheet has no effect without rtmp_stat."));
        }
        _ => {}
    }
    out
}

/// `deny all` written before an `allow` of the same scope hides it: the
/// first rule that matches wins.
fn access_order(items: &[Item]) -> Vec<Hint> {
    let rule = |i: &Item| -> Option<(bool, Option<String>, String)> {
        let Item::Dir { name, args, block: None, .. } = i else { return None };
        let allow = match name.as_str() {
            "allow" => true,
            "deny" => false,
            _ => return None,
        };
        match args.as_slice() {
            [scope, addr] => Some((allow, Some(scope.clone()), addr.clone())),
            [addr] => Some((allow, None, addr.clone())),
            _ => None,
        }
    };
    let rules: Vec<_> = items.iter().filter_map(rule).collect();
    let mut out = Vec::new();
    for (k, (allow, scope, addr)) in rules.iter().enumerate() {
        if !allow {
            continue;
        }
        let blocked = rules[..k].iter().any(|(a, s, ad)| !a && ad == "all" && (s.is_none() || s == scope));
        if blocked {
            let what = scope.as_deref().map_or(String::new(), |s| format!("{s} "));
            out.push(hint(
                Tone::Warn,
                format!("allow {what}{addr} comes after a deny … all, so it never matches: move it up (the first matching rule wins)."),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------- recipes

/// A ready-made snippet for a common setup.
pub struct Recipe {
    pub name: &'static str,
    pub help: &'static str,
    /// The block it goes in.
    pub ctx: &'static str,
    pub text: &'static str,
}

const STAT_LOCATIONS: &str = "location /stat {\n    rtmp_stat all;\n    rtmp_stat_stylesheet stat.xsl;\n}\nlocation /stat.xsl {\n    root html;\n}\n";

pub const RECIPES: &[Recipe] = &[
    Recipe {
        name: "RTMP server",
        help: "A server on port 1935 with a live application.",
        ctx: "rtmp",
        text: "server {\n    listen 1935;\n    application live {\n        live on;\n    }\n}\n",
    },
    Recipe {
        name: "Live application",
        help: "Publish to rtmp://host/live/<stream> and play the same address.",
        ctx: "server",
        text: "application live {\n    live on;\n}\n",
    },
    Recipe {
        name: "Live application with recording",
        help: "Like Live application, and every stream is saved to recordings/.",
        ctx: "server",
        text: "application live {\n    live on;\n    record all;\n    record_path recordings;\n    record_unique on;\n}\n",
    },
    Recipe {
        name: "Record to disk",
        help: "Save every published stream as an .flv file in recordings/.",
        ctx: "application",
        text: "record all;\nrecord_path recordings;\nrecord_unique on;\n",
    },
    Recipe {
        name: "Relay to YouTube",
        help: "Send published streams to YouTube Live; replace STREAM-KEY with your key.",
        ctx: "application",
        text: "push rtmp://a.rtmp.youtube.com/live2/STREAM-KEY;\n",
    },
    Recipe {
        name: "Relay to Twitch",
        help: "Send published streams to Twitch; replace STREAM-KEY with your key.",
        ctx: "application",
        text: "push rtmp://live.twitch.tv/app/STREAM-KEY;\n",
    },
    Recipe {
        name: "Publish from this computer only",
        help: "Anyone may play; only 127.0.0.1 may publish.",
        ctx: "application",
        text: "allow publish 127.0.0.1;\ndeny publish all;\n",
    },
    Recipe {
        name: "Check publishers over HTTP",
        help: "Before a stream starts, ask your web app; a 2xx answer accepts it, anything else rejects it.",
        ctx: "application",
        text: "on_publish http://127.0.0.1:8080/on_publish;\n",
    },
    Recipe {
        name: "Statistics page",
        help: "http://localhost:8080/stat shows the streams and clients.",
        ctx: "main",
        text: "http {\n    server {\n        listen 8080;\n        location /stat {\n            rtmp_stat all;\n            rtmp_stat_stylesheet stat.xsl;\n        }\n        location /stat.xsl {\n            root html;\n        }\n    }\n}\n",
    },
    Recipe {
        name: "Statistics page",
        help: "/stat on this server shows the streams and clients.",
        ctx: "http_server",
        text: STAT_LOCATIONS,
    },
    Recipe {
        name: "Remote control",
        help: "/control on this server starts and stops recordings and drops clients.",
        ctx: "http_server",
        text: "location /control {\n    rtmp_control all;\n}\n",
    },
];

/// The recipes (indexes in [`RECIPES`]) for a block in context `ctx`
/// holding `items`.
pub fn recipes(ctx: &str, items: &[Item]) -> Vec<usize> {
    (0..RECIPES.len()).filter(|&k| RECIPES[k].ctx == ctx && !(ctx == "main" && has(items, "http"))).collect()
}

/// Add `recipe` to `items` (a block in context `ctx`): directives written
/// once at most that are already there get the recipe's value instead.
pub fn apply(specs: &[Spec], items: &mut Vec<Item>, ctx: &str, recipe: &Recipe) {
    let Ok(new) = conf_edit::parse(recipe.text) else { return };
    for it in new {
        if let Item::Dir { name, args, block: None, .. } = &it {
            let single = form::spec_for(specs, name, ctx).is_some_and(|s| !s.multi);
            let same = items.iter_mut().find(|i| matches!(i, Item::Dir { name: n, .. } if n == name));
            if let (true, Some(old)) = (single, same) {
                form::set_args(old, args.clone());
                continue;
            }
        }
        items.push(it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conf_edit::parse;
    use crate::conf_schema::specs;

    fn spec(name: &str, ctx: &str) -> Spec {
        specs().into_iter().find(|s| s.name == name && s.ctx.contains(&ctx)).expect(name)
    }

    #[test]
    fn field_problems() {
        let t = spec("timeout", "server");
        assert_eq!(field_problem(&t, &t.args[0], ""), Some("Required."));
        assert!(field_problem(&t, &t.args[0], "30x").is_some());
        assert_eq!(field_problem(&t, &t.args[0], "30s"), None);
        let p = spec("push", "application");
        assert!(field_problem(&p, &p.args[0], "http://x/app").is_some());
        assert_eq!(field_problem(&p, &p.args[0], "rtmps://x/app"), None);
        let n = spec("on_publish", "application");
        assert!(field_problem(&n, &n.args[0], "rtmp://x").is_some());
        assert_eq!(field_problem(&n, &n.args[0], "http://127.0.0.1:8080/p"), None);
    }

    #[test]
    fn messages_on_directives() {
        let tree = parse("rtmp {\n    server {\n        listen 1936 ssl;\n    }\n}\n").unwrap();
        let text = conf_edit::render(&tree);
        let check = Check::of(&text, "f.conf");
        assert!(!check.ok);
        let at = locate(&check, "f.conf", &tree);
        assert_eq!(at.len(), 1, "{check:?}");
        assert_eq!(at[0].0, vec![0, 0]);
        assert_eq!(at[0].1, Tone::Err);
        assert!(at[0].2.starts_with("no \"ssl_certificate\""));
        assert_eq!(split_location("C:/a/b.conf:12: oops: x", "C:/a/b.conf"), Some((12, "oops: x")));
    }

    #[test]
    fn inherited_and_overridden() {
        let specs = specs();
        let tree = parse("rtmp {\n chunk_size 8192;\n live on;\n server {\n  timeout 5s;\n  application a {\n   live off;\n  }\n }\n}\n")
            .unwrap();
        let got: Vec<(String, String)> =
            inherited(&specs, &tree, &[0, 2, 1]).into_iter().map(|i| (i.name, i.from)).collect();
        // chunk_size and timeout are server settings, live is set here
        assert!(got.is_empty(), "{got:?}");
        let srv: Vec<String> = inherited(&specs, &tree, &[0, 2]).into_iter().map(|i| i.name).collect();
        assert_eq!(srv, vec!["chunk_size", "live"]);
        assert_eq!(overrides(&tree, &[0, 2, 1], "live"), Some((vec!["on".into()], "rtmp".into())));
        assert_eq!(overrides(&tree, &[0, 2, 1], "sync"), None);
    }

    #[test]
    fn defaults() {
        assert_eq!(default_args(&spec("chunk_size", "server")), Some(vec!["4096".into()]));
        assert_eq!(default_args(&spec("out_cork", "server")), None);
        assert_eq!(default_args(&spec("record_path", "application")), None);
        assert_eq!(default_args(&spec("live", "application")), Some(vec!["off".into()]));
    }

    #[test]
    fn hints_and_fixes() {
        let tree = parse("rtmp {\n server {\n  listen 1936 ssl;\n  application a {\n   record_path /r;\n   deny publish all;\n   allow publish 127.0.0.1;\n  }\n }\n}\n")
            .unwrap();
        let srv = hints(&tree, &[0, 0]);
        assert_eq!(srv[0].tone, Tone::Err);
        assert_eq!(srv[0].fix.as_ref().unwrap().add.len(), 2);
        let app: Vec<String> = hints(&tree, &[0, 0, 1]).into_iter().map(|h| h.text).collect();
        assert!(app.iter().any(|t| t.starts_with("live is off")), "{app:?}");
        assert!(app.iter().any(|t| t.starts_with("record is off")), "{app:?}");
        assert!(app.iter().any(|t| t.starts_with("allow publish 127.0.0.1")), "{app:?}");
        let ok = parse("rtmp { live on; server { application a { record all; record_path r; } } }").unwrap();
        assert!(hints(&ok, &[0, 1, 0]).is_empty(), "{:?}", hints(&ok, &[0, 1, 0]));
    }

    /// Every recipe is accepted by the server where it is offered.
    #[test]
    fn recipes_parse() {
        let specs = specs();
        let around = |ctx: &str| -> (&str, &str) {
            match ctx {
                "main" => ("", " rtmp { }"),
                "rtmp" => ("rtmp {", "}"),
                "server" => ("rtmp { server {", "} }"),
                "application" => ("rtmp { server { application a { live on;", "} } }"),
                "http_server" => ("rtmp { } http { server {", "} }"),
                _ => unreachable!(),
            }
        };
        for r in RECIPES {
            let (a, b) = around(r.ctx);
            let src = format!("{a}\n{}\n{b}", r.text);
            let c = crate::config::Config::parse(&src, "t").unwrap_or_else(|e| panic!("{}: {e}", r.name));
            assert!(c.warnings.is_empty(), "{}: {:?}", r.name, c.warnings);
        }
        let mut items = parse("record off;\n").unwrap();
        apply(&specs, &mut items, "application", RECIPES.iter().find(|r| r.name == "Record to disk").unwrap());
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], Item::Dir { args, .. } if args == &["all"]));
    }
}
