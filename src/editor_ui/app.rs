//! The editor window: the configuration as a tree of blocks with a form
//! for each directive, or as text. The same editor as the web page
//! (`editor.html`), drawn with iced.

use super::api::{Api, Content, Loaded, Saved};
use super::style;
use iced::widget::{
    Space, button, checkbox, column, container, opaque, pick_list, row, rule, scrollable, stack, text, text_editor,
    text_input, tooltip,
};
use iced::{Alignment, Border, Color, Element, Font, Length, Subscription, Task, Theme, event, keyboard, window};
use nginx_rtmp::conf_edit::{self, Item};
use nginx_rtmp::conf_form::{self as form, Check, Level, Val};
use nginx_rtmp::conf_help::{self as help, Tone};
use nginx_rtmp::conf_schema::{Arg, Kind, Spec};
use std::collections::HashMap;
use std::io::Write;

pub const TITLE: &str = "nginx configuration";

/// Index path of an item: the path of its block, then its index there.
type ItemPath = Vec<usize>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Form,
    Text,
}

/// A question shown over the window.
#[derive(Debug, Clone)]
enum Dialog {
    DeleteBlock(ItemPath),
    Overwrite,
    Discard,
    ReloadUnsaved,
    Close,
}

#[derive(Debug, Clone)]
enum Msg {
    Opened(window::Id),
    RawId(u64),
    CloseRequested,
    Loaded(Result<Loaded, String>),
    Saved(Saved),
    Reloaded(Result<(bool, String), String>),
    Select(Vec<usize>),
    SetMode(Mode),
    // fields of the directive at a path
    SetOne(ItemPath, usize, String),
    SetList(ItemPath, usize, String),
    Toggle(ItemPath, usize, String, bool),
    ParamKey(ItemPath, usize, usize, String),
    ParamValue(ItemPath, usize, usize, String),
    ParamAdd(ItemPath, usize),
    ParamRemove(ItemPath, usize, usize),
    Generic(ItemPath, String),
    CommentText(ItemPath, String),
    /// Start (`Some`) or stop editing a comment.
    EditComment(Option<ItemPath>),
    // rows
    Move(ItemPath, isize),
    Delete(ItemPath),
    Disable(ItemPath),
    Enable(ItemPath),
    // adding
    /// The text of the "add" search box.
    AddQuery(String),
    /// Add a known directive by name.
    Add(&'static str),
    /// Enter in the search box: the only match, or the text as written.
    AddSubmit,
    AddComment,
    /// Add a ready-made snippet (index in `help::RECIPES`) to the open block.
    Recipe(usize),
    /// Add these items to the open block (the fix of a hint).
    Fix(Vec<Item>),
    /// Give the open block its own copy of an inherited setting.
    Override(String, Vec<String>),
    /// Replace the arguments of the directive at a path.
    SetArgs(ItemPath, Vec<String>),
    // text view
    Edit(text_editor::Action),
    // header
    Save,
    Revert,
    ReloadServer,
    Answer(bool),
}

struct Editor {
    api: Api,
    specs: Vec<Spec>,
    file: String,
    hash: String,
    tree: Vec<Item>,
    /// The block shown in the form.
    sel: Vec<usize>,
    mode: Mode,
    content: text_editor::Content,
    dirty: bool,
    loaded: bool,
    check: Option<Check>,
    /// The check's messages that name a directive, by its path.
    located: Vec<(ItemPath, Tone, String)>,
    status: Vec<(Level, String)>,
    /// Shown first after the next load (the outcome of a save).
    pending: Option<(Level, String)>,
    busy: bool,
    dialog: Option<Dialog>,
    /// What is typed in the "add" search box.
    query: String,
    /// Text typed in fields that hold several arguments, kept as typed
    /// (re-joining the arguments would eat a trailing space).
    drafts: HashMap<(ItemPath, usize), String>,
    /// The comment being edited.
    editing: Option<ItemPath>,
}

/// Field number of the plain "arguments" field of unknown directives.
const GENERIC: usize = usize::MAX;

pub fn run(api: Api) -> iced::Result {
    let theme = style::init();
    iced::application(
        move || {
            let e = Editor::new(api.clone());
            let load = e.load();
            (e, load)
        },
        Editor::update,
        Editor::view,
    )
    .settings(iced::Settings { default_text_size: 14.into(), ..iced::Settings::default() })
    .theme(theme)
    .title(Editor::title)
    .subscription(Editor::subscription)
    .exit_on_close_request(false)
    .window(window::Settings {
        size: iced::Size::new(1100.0, 780.0),
        min_size: Some(iced::Size::new(640.0, 420.0)),
        position: window::Position::Centered,
        exit_on_close_request: false,
        ..window::Settings::default()
    })
    .run()
}

impl Editor {
    fn new(api: Api) -> Editor {
        Editor {
            api,
            specs: nginx_rtmp::conf_schema::specs(),
            file: String::new(),
            hash: String::new(),
            tree: Vec::new(),
            sel: Vec::new(),
            mode: Mode::Form,
            content: text_editor::Content::new(),
            dirty: false,
            loaded: false,
            check: None,
            located: Vec::new(),
            status: vec![(Level::Ok, "Loading…".into())],
            pending: None,
            busy: false,
            dialog: None,
            query: String::new(),
            drafts: HashMap::new(),
            editing: None,
        }
    }

    fn title(&self) -> String {
        match (self.file.is_empty(), self.dirty) {
            (true, _) => TITLE.into(),
            (false, false) => format!("{} - {TITLE}", self.file),
            (false, true) => format!("• {} - {TITLE}", self.file),
        }
    }

    fn subscription(&self) -> Subscription<Msg> {
        Subscription::batch([
            window::open_events().map(Msg::Opened),
            window::close_requests().map(|_| Msg::CloseRequested),
            event::listen_with(|e, _, _| match e {
                iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                    if modifiers.command() && key.as_ref() == keyboard::Key::Character("s") =>
                {
                    Some(Msg::Save)
                }
                _ => None,
            }),
        ])
    }

    // ------------------------------------------------------------ tasks

    fn load(&self) -> Task<Msg> {
        let api = self.api.clone();
        Task::perform(async move { api.load() }, Msg::Loaded)
    }

    fn save(&mut self, force: bool) -> Task<Msg> {
        if self.busy || !self.loaded {
            return Task::none();
        }
        self.busy = true;
        let api = self.api.clone();
        let hash = self.hash.clone();
        let content = match self.mode {
            Mode::Form => Content::Tree(self.tree.clone()),
            Mode::Text => Content::Text(self.content.text()),
        };
        Task::perform(async move { api.save(content, &hash, force) }, Msg::Saved)
    }

    fn reload_server(&mut self) -> Task<Msg> {
        let api = self.api.clone();
        Task::perform(async move { api.reload() }, Msg::Reloaded)
    }

    // ----------------------------------------------------------- update

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Opened(id) => {
                println!("{}", super::READY);
                let _ = std::io::stdout().flush();
                return Task::batch([window::gain_focus(id), window::raw_id::<Msg>(id).map(Msg::RawId)]);
            }
            Msg::RawId(id) => super::set_icon(id),
            Msg::CloseRequested => {
                if self.dirty {
                    self.dialog = Some(Dialog::Close);
                } else {
                    return iced::exit();
                }
            }
            Msg::Loaded(r) => self.loaded(r),
            Msg::Saved(r) => {
                self.busy = false;
                match r {
                    Saved::Conflict => self.dialog = Some(Dialog::Overwrite),
                    Saved::Failed(e) => self.status = vec![(Level::Err, e)],
                    Saved::Done { ok, message } => {
                        self.pending = Some(if ok {
                            (Level::Ok, format!("Saved. {message}"))
                        } else {
                            (Level::Err, format!("Saved, but the server was not reloaded: {message}"))
                        });
                        return self.load();
                    }
                }
            }
            Msg::Reloaded(r) => {
                let extra = match r {
                    Ok((true, m)) => (Level::Ok, m),
                    Ok((false, m)) | Err(m) => (Level::Err, m),
                };
                self.show(Some(extra));
            }
            Msg::Select(p) => {
                self.sel = p;
                self.editing = None;
                self.drafts.clear();
                self.query.clear();
                return iced::widget::operation::snap_to(PANE, iced::widget::operation::RelativeOffset::START);
            }
            Msg::SetMode(m) => self.set_mode(m),
            Msg::SetOne(ip, n, v) => self.edit_vals(&ip, |_, vals| vals[n] = Val::One(v)),
            Msg::SetList(ip, n, v) => {
                let args = form::split_args(&v);
                self.drafts.insert((ip.clone(), n), v);
                self.edit_vals(&ip, |_, vals| vals[n] = Val::Many(args));
            }
            Msg::Toggle(ip, n, value, on) => self.edit_vals(&ip, |spec, vals| {
                let Kind::Choices(known) = spec.args[n].kind else { return };
                let mut cur = vals[n].as_list();
                cur.retain(|v| v != &value && !form::same_key(v, &value));
                if on {
                    cur.push(value);
                }
                // keep the order of the description, unknown values last
                let mut order: Vec<String> = known.iter().map(|s| s.to_string()).collect();
                order.extend(cur.iter().filter(|v| !known.contains(&v.as_str())).cloned());
                order.retain(|v| cur.contains(v));
                order.dedup();
                vals[n] = Val::Many(order);
            }),
            Msg::ParamKey(ip, n, j, key) => self.edit_vals(&ip, |_, vals| {
                let mut list = vals[n].as_list();
                if let Some(kv) = list.get_mut(j) {
                    *kv = format!("{key}={}", form::split_param(kv).1);
                }
                vals[n] = Val::Many(list);
            }),
            Msg::ParamValue(ip, n, j, value) => self.edit_vals(&ip, |_, vals| {
                let mut list = vals[n].as_list();
                if let Some(kv) = list.get_mut(j) {
                    *kv = format!("{}={value}", form::split_param(kv).0);
                }
                vals[n] = Val::Many(list);
            }),
            Msg::ParamAdd(ip, n) => self.edit_vals(&ip, |spec, vals| {
                let Kind::Param(keys) = spec.args[n].kind else { return };
                let mut list = vals[n].as_list();
                list.push(format!("{}=", keys.first().copied().unwrap_or("key")));
                vals[n] = Val::Many(list);
            }),
            Msg::ParamRemove(ip, n, j) => self.edit_vals(&ip, |_, vals| {
                let mut list = vals[n].as_list();
                if j < list.len() {
                    list.remove(j);
                }
                vals[n] = Val::Many(list);
            }),
            Msg::Generic(ip, v) => {
                let args = form::split_args(&v);
                self.drafts.insert((ip.clone(), GENERIC), v);
                self.with_item(&ip, |it| form::set_args(it, args));
            }
            Msg::EditComment(ip) => {
                let start = ip.is_some();
                self.editing = ip;
                if start {
                    return iced::widget::operation::focus(COMMENT);
                }
            }
            Msg::CommentText(ip, v) => self.with_item(&ip, |it| {
                if let Item::Comment { text } = it {
                    *text = v;
                }
            }),
            Msg::Move(ip, d) => {
                let (parent, i) = split(&ip);
                if let Some(items) = form::items_at_mut(&mut self.tree, parent) {
                    form::move_item(items, i, d);
                }
                self.structure_changed(parent.to_vec());
            }
            Msg::Delete(ip) => {
                if matches!(self.item(&ip), Some(Item::Dir { block: Some(_), .. })) {
                    self.dialog = Some(Dialog::DeleteBlock(ip));
                } else {
                    self.delete(&ip);
                }
            }
            Msg::Disable(ip) => {
                let new = self.item(&ip).and_then(form::disable);
                if let Some(new) = new {
                    self.with_item(&ip, |it| *it = new);
                    self.drafts.clear();
                }
            }
            Msg::Enable(ip) => {
                let new = match self.item(&ip) {
                    Some(Item::Comment { text }) => form::enable(text),
                    _ => None,
                };
                if let Some(new) = new {
                    self.with_item(&ip, |it| *it = new);
                    self.drafts.clear();
                }
            }
            Msg::AddQuery(v) => self.query = v,
            Msg::Add(name) => return self.add(name),
            Msg::AddSubmit => {
                let q = self.query.trim().trim_end_matches(';').trim();
                let found: Vec<&'static str> = self.matches().iter().flat_map(|(_, l)| l.iter().map(|s| s.name)).collect();
                // a bare known name, or the only match of a search word;
                // with arguments typed, the text as written
                let known = if q.contains(char::is_whitespace) {
                    None
                } else if found.contains(&q) {
                    found.iter().copied().find(|n| *n == q)
                } else if found.len() == 1 {
                    Some(found[0])
                } else {
                    None
                };
                match known {
                    Some(name) => return self.add(name),
                    // a search word that matches several: wait for a click
                    None if !q.contains(char::is_whitespace) && !found.is_empty() => {}
                    None => {
                        if let Some(it) = form::parse_custom(&self.query) {
                            self.query.clear();
                            self.push_item(it);
                        }
                    }
                }
            }
            Msg::AddComment => self.push_item(Item::Comment { text: " ".into() }),
            Msg::Recipe(k) => {
                let ctx = form::ctx_of(&self.tree, &self.sel);
                if let (Some(recipe), Some(items)) = (help::RECIPES.get(k), form::items_at_mut(&mut self.tree, &self.sel)) {
                    help::apply(&self.specs, items, ctx, recipe);
                    self.structure_changed(self.sel.clone());
                }
            }
            Msg::Fix(add) => {
                if let Some(items) = form::items_at_mut(&mut self.tree, &self.sel) {
                    items.extend(add);
                    self.structure_changed(self.sel.clone());
                }
            }
            Msg::Override(name, args) => self.push_item(form::dir(&name, args, None)),
            Msg::SetArgs(ip, args) => {
                self.drafts.retain(|(p, _), _| p != &ip);
                self.with_item(&ip, |it| form::set_args(it, args));
            }
            Msg::Edit(action) => {
                let edit = action.is_edit();
                self.content.perform(action);
                if edit {
                    self.dirty = true;
                    self.check = Some(Check::of(&self.content.text(), &self.file));
                    self.show(None);
                }
            }
            Msg::Save => return self.save(false),
            Msg::Revert => {
                if self.dirty {
                    self.dialog = Some(Dialog::Discard);
                } else {
                    return self.revert();
                }
            }
            Msg::ReloadServer => {
                if self.dirty {
                    self.dialog = Some(Dialog::ReloadUnsaved);
                } else {
                    return self.reload_server();
                }
            }
            Msg::Answer(yes) => {
                let Some(d) = self.dialog.take() else { return Task::none() };
                if yes {
                    match d {
                        Dialog::DeleteBlock(ip) => self.delete(&ip),
                        Dialog::Overwrite => return self.save(true),
                        Dialog::Discard => return self.revert(),
                        Dialog::ReloadUnsaved => return self.reload_server(),
                        Dialog::Close => return iced::exit(),
                    }
                }
            }
        }
        Task::none()
    }

    fn loaded(&mut self, r: Result<Loaded, String>) {
        let l = match r {
            Ok(l) => l,
            Err(e) => {
                self.status = vec![(Level::Err, e)];
                self.pending = None;
                return;
            }
        };
        self.loaded = true;
        self.file = l.path;
        self.hash = l.hash;
        self.dirty = false;
        self.drafts.clear();
        self.content = text_editor::Content::with_text(&l.text);
        match l.tree {
            Ok(tree) => {
                self.tree = tree;
                self.sel = form::valid_path(&self.tree, &self.sel);
                self.check = Some(Check::of(&l.text, &self.file));
                let extra = self.pending.take();
                self.show(extra);
            }
            Err(e) => {
                self.mode = Mode::Text;
                self.check = None;
                self.pending = None;
                self.status = vec![(Level::Err, format!("The file cannot be shown as a form: {e}"))];
            }
        }
    }

    fn revert(&mut self) -> Task<Msg> {
        self.mode = Mode::Form;
        self.load()
    }

    fn set_mode(&mut self, m: Mode) {
        if m == self.mode {
            return;
        }
        match m {
            Mode::Text => self.content = text_editor::Content::with_text(&conf_edit::render(&self.tree)),
            Mode::Form => match conf_edit::parse(&self.content.text()) {
                Ok(tree) => {
                    self.tree = tree;
                    self.sel = form::valid_path(&self.tree, &self.sel);
                    self.drafts.clear();
                    self.recheck();
                }
                Err(e) => {
                    self.status = vec![(Level::Err, format!("Fix the text first: {e}"))];
                    return;
                }
            },
        }
        self.mode = m;
        if m == Mode::Form {
            self.show(None);
        }
    }

    /// Status lines: `extra` first, then the check (its "valid" line only
    /// without `extra`).
    fn show(&mut self, extra: Option<(Level, String)>) {
        let mut out: Vec<(Level, String)> = extra.clone().into_iter().collect();
        if let Some(c) = &self.check {
            out.extend(form::check_messages(c, self.dirty).into_iter().filter(|(l, _)| extra.is_none() || *l != Level::Ok));
        }
        self.status = out;
        self.located = match (&self.check, self.mode) {
            (Some(c), Mode::Form) => help::locate(c, &self.file, &self.tree),
            _ => Vec::new(),
        };
    }

    fn recheck(&mut self) {
        self.check = Some(Check::of(&conf_edit::render(&self.tree), &self.file));
        self.show(None);
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.recheck();
    }

    /// After items of the block at `parent` were added, moved or removed.
    fn structure_changed(&mut self, parent: Vec<usize>) {
        self.drafts.clear();
        self.editing = None;
        if self.sel.len() > parent.len() && self.sel.starts_with(&parent) {
            self.sel = parent;
        }
        self.changed();
    }

    fn item(&self, ip: &[usize]) -> Option<&Item> {
        let (parent, i) = split(ip);
        form::items_at(&self.tree, parent)?.get(i)
    }

    fn with_item(&mut self, ip: &[usize], f: impl FnOnce(&mut Item)) {
        let (parent, i) = split(ip);
        if let Some(it) = form::items_at_mut(&mut self.tree, parent).and_then(|items| items.get_mut(i)) {
            f(it);
            self.changed();
        }
    }

    /// Change the fields of the directive at `ip`.
    fn edit_vals(&mut self, ip: &[usize], f: impl FnOnce(&Spec, &mut Vec<Val>)) {
        let (parent, _) = split(ip);
        let ctx = form::ctx_of(&self.tree, parent);
        let Some(Item::Dir { name, args, .. }) = self.item(ip) else { return };
        let Some(spec) = form::spec_for(&self.specs, name, ctx) else { return };
        let mut vals = form::assign(spec, args);
        f(spec, &mut vals);
        let new = form::rebuild(spec, &vals);
        self.with_item(ip, |it| form::set_args(it, new));
    }

    fn delete(&mut self, ip: &[usize]) {
        let (parent, i) = split(ip);
        if let Some(items) = form::items_at_mut(&mut self.tree, parent).filter(|items| i < items.len()) {
            items.remove(i);
        }
        self.structure_changed(parent.to_vec());
    }

    fn push_item(&mut self, it: Item) {
        if let Some(items) = form::items_at_mut(&mut self.tree, &self.sel) {
            items.push(it);
        }
        self.structure_changed(self.sel.clone());
    }

    /// Add the directive `name` to the open block; a new block without
    /// arguments to fill is opened.
    fn add(&mut self, name: &str) -> Task<Msg> {
        let ctx = form::ctx_of(&self.tree, &self.sel);
        let Some(spec) = form::spec_for(&self.specs, name, ctx) else { return Task::none() };
        let Some(items) = form::items_at_mut(&mut self.tree, &self.sel) else { return Task::none() };
        let (at, open) = form::add(items, spec);
        self.query.clear();
        self.structure_changed(self.sel.clone());
        if open {
            let mut p = self.sel.clone();
            p.push(at);
            return self.update(Msg::Select(p));
        }
        Task::none()
    }

    /// The directives the open block accepts that match the search box,
    /// by module; names that match come before descriptions that do.
    fn matches(&self) -> Vec<(&'static str, Vec<&Spec>)> {
        let items = form::items_at(&self.tree, &self.sel).unwrap_or(&[]);
        let ctx = form::ctx_of(&self.tree, &self.sel);
        let all = form::available(&self.specs, items, ctx);
        let q = self.query.trim().to_lowercase();
        let word = q.split_whitespace().next().unwrap_or("");
        if word.is_empty() {
            return all;
        }
        // tiers: name starts with the word, name contains it, description
        // contains the search; with arguments typed, only the exact name
        let args = q.contains(char::is_whitespace);
        let tier = |s: &Spec| -> Option<u8> {
            if args {
                return (s.name == word).then_some(0);
            }
            if s.name.starts_with(word) {
                Some(0)
            } else if s.name.contains(word) {
                Some(1)
            } else if s.help.to_lowercase().contains(&q) {
                Some(2)
            } else {
                None
            }
        };
        let mut out: Vec<(&'static str, Vec<&Spec>)> = Vec::new();
        for t in 0..3 {
            for (m, list) in &all {
                let hit: Vec<&Spec> = list.iter().copied().filter(|s| tier(s) == Some(t)).collect();
                if hit.is_empty() {
                    continue;
                }
                match out.iter_mut().find(|(o, _)| o == m) {
                    Some((_, l)) => l.extend(hit),
                    None => out.push((*m, hit)),
                }
            }
        }
        out
    }

    // ------------------------------------------------------------- view

    fn view(&self) -> Element<'_, Msg> {
        let c = style::colors();
        let body: Element<'_, Msg> = match self.mode {
            Mode::Form => row![
                container(scrollable(self.tree_view()).height(Length::Fill))
                    .width(250)
                    .height(Length::Fill)
                    .style(style::sidebar),
                vline(),
                scrollable(container(self.pane()).padding([20, 28]).max_width(1040))
                    .id(PANE)
                    .width(Length::Fill)
                    .height(Length::Fill),
            ]
            .height(Length::Fill)
            .into(),
            Mode::Text => container(
                text_editor(&self.content)
                    .on_action(Msg::Edit)
                    .font(Font::MONOSPACE)
                    .size(13)
                    .height(Length::Fill)
                    .padding(14)
                    .style(move |t, s| {
                        let mut st = text_editor::default(t, s);
                        st.background = c.surface.into();
                        st.border = Border { radius: 10.0.into(), width: 1.0, color: c.border };
                        st.selection = Color { a: 0.3, ..c.accent };
                        st
                    }),
            )
            .padding([16, 20])
            .height(Length::Fill)
            .into(),
        };
        let page = container(column![self.header(), hline(), body, hline(), self.footer()])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(style::page);
        // Always a stack, so the page keeps its place (and its scroll
        // position) when a dialog opens over it.
        let mut layers = stack![page];
        if let Some(d) = &self.dialog {
            layers = layers.push(self.dialog_view(d));
        }
        layers.into()
    }

    fn header(&self) -> Element<'_, Msg> {
        let c = style::colors();
        let tab = |label, m: Mode| {
            button(text(label).size(13)).padding([5, 14]).style(style::tab(self.mode == m)).on_press(Msg::SetMode(m))
        };
        let mut title = row![text("nginx.conf").size(17).font(style::SEMIBOLD)].spacing(8).align_y(Alignment::Center);
        if self.dirty {
            title = title.push(pill("unsaved", style::pill_warn));
        }
        // A long path is cut off rather than pushing the buttons out.
        let name = column![title, text(&self.file).size(12).color(c.muted).wrapping(text::Wrapping::None)].spacing(2);
        let bar = row![
            container(name).width(Length::Fill).clip(true),
            container(row![tab("Form", Mode::Form), tab("Text", Mode::Text)].spacing(2)).padding(3).style(style::segmented),
            Space::new().width(8),
            with_tip(
                button(text("Revert").size(13))
                    .padding([7, 14])
                    .style(style::secondary)
                    .on_press_maybe(self.loaded.then_some(Msg::Revert)),
                "Discard changes and load the file again",
            ),
            with_tip(
                button(text("Reload server").size(13)).padding([7, 14]).style(style::secondary).on_press(Msg::ReloadServer),
                "Reload the file as it is on disk into the running server",
            ),
            with_tip(
                button(text(if self.busy { "Saving…" } else { "Save & reload" }).size(13).font(style::SEMIBOLD))
                    .padding([7, 16])
                    .style(style::primary)
                    .on_press_maybe((!self.busy && self.loaded).then_some(Msg::Save)),
                "Check, save and reload the running server (Ctrl+S)",
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .padding([12, 20]);
        container(bar).width(Length::Fill).style(style::bar).into()
    }

    fn footer(&self) -> Element<'_, Msg> {
        let c = style::colors();
        let lines = self.status.iter().map(|(l, m)| {
            let color = match l {
                Level::Ok => c.success,
                Level::Warn => c.warning,
                Level::Err => c.danger,
            };
            row![text("●").size(10).color(color), text(m.as_str()).size(13)].spacing(8).align_y(Alignment::Center).into()
        });
        container(scrollable(column(lines).spacing(4)).height(Length::Shrink))
            .max_height(110)
            .padding([9, 20])
            .width(Length::Fill)
            .style(style::bar)
            .into()
    }

    fn tree_view(&self) -> Element<'_, Msg> {
        let c = style::colors();
        let node = |depth: usize, name: String, detail: String, path: Vec<usize>| -> Element<'_, Msg> {
            let on = path == self.sel;
            row![
                Space::new().width((depth * 14) as f32),
                button(
                    row![
                        text(name).size(14).wrapping(text::Wrapping::None),
                        text(detail).size(12).color(if on { c.accent } else { c.muted }).wrapping(text::Wrapping::None),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([6, 10])
                .style(style::node(on))
                .on_press(Msg::Select(path)),
            ]
            .into()
        };
        let mut col = column![
            container(text("BLOCKS").size(11).font(style::SEMIBOLD).color(c.muted)).padding([4, 10]),
            node(0, "nginx.conf".into(), "file".into(), Vec::new()),
        ]
        .spacing(2)
        .padding([14, 10]);
        for n in form::blocks(&self.tree) {
            col = col.push(node(n.depth, n.name, n.detail, n.path));
        }
        col.into()
    }

    fn pane(&self) -> Element<'_, Msg> {
        let c = style::colors();
        let tree = &self.tree;
        let ctx = form::ctx_of(tree, &self.sel);
        let Some(items) = form::items_at(tree, &self.sel) else { return text("").into() };
        let me = form::item_at(tree, &self.sel);
        let title = match me {
            Some(Item::Dir { name, args, .. }) => format!("{name} {}", args.join(" ")).trim().to_string(),
            _ => "nginx.conf".into(),
        };
        let mut col = column![
            text(form::crumbs(tree, &self.sel).join("  ›  ")).size(12).color(c.muted),
            row![text(title).size(24).font(style::SEMIBOLD), pill(form::ctx_label(ctx), style::pill)]
                .spacing(12)
                .align_y(Alignment::Center),
        ]
        .spacing(8);
        match me {
            Some(it @ Item::Dir { name, args, .. }) => {
                let parent_ctx = form::ctx_of(tree, &self.sel[..self.sel.len() - 1]);
                match form::spec_for(&self.specs, name, parent_ctx) {
                    Some(spec) => {
                        col = col.push(text(spec.help).color(c.muted));
                        if !spec.args.is_empty() {
                            col = col.push(Space::new().height(4)).push(
                                container(self.fields(self.sel.clone(), spec, args)).padding([12, 14]).style(style::card),
                            );
                        }
                    }
                    None if !args.is_empty() => {
                        col = col.push(container(self.generic(self.sel.clone(), it)).padding([12, 14]).style(style::card))
                    }
                    None => {}
                }
            }
            _ => col = col.push(text("Top level of the file. Open a block on the left, or add one below.").color(c.muted)),
        }
        if let Some(n) = self.notices() {
            col = col.push(Space::new().height(4)).push(n);
        }
        let count = form::directive_count(items);
        col = col.push(Space::new().height(10)).push(
            row![
                text("Directives").size(15).font(style::SEMIBOLD),
                text(count.to_string()).size(13).color(c.muted),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        let mut rows = column![].spacing(8);
        for (i, it) in items.iter().enumerate() {
            let mut ip = self.sel.clone();
            ip.push(i);
            rows = rows.push(self.item_row(ip, it, items, ctx));
        }
        if items.iter().all(|i| matches!(i, Item::Blank)) {
            rows = rows.push(text("Nothing here yet.").color(c.muted));
        }
        col = col.push(rows).push(Space::new().height(6));
        if let Some(card) = self.inherited_card() {
            col = col.push(card);
        }
        col = col.push(self.add_bar());
        if let Some(Item::Dir { name, .. }) = me {
            col = col.push(Space::new().height(6)).push(
                button(text(format!("Delete this {name} block")).size(13))
                    .padding([7, 14])
                    .style(style::danger)
                    .on_press(Msg::Delete(self.sel.clone())),
            );
        }
        col.into()
    }

    fn item_row<'a>(&'a self, ip: ItemPath, it: &'a Item, items: &'a [Item], ctx: &str) -> Element<'a, Msg> {
        let c = style::colors();
        let i = *ip.last().unwrap_or(&0);
        let tools = |extra: Vec<Element<'static, Msg>>| -> Element<'static, Msg> {
            let mut r = row![].spacing(0).align_y(Alignment::Center);
            for e in extra {
                r = r.push(e);
            }
            r.push(icon("↑", "Move up", form::can_move(items, i, -1).then(|| Msg::Move(ip.clone(), -1))))
                .push(icon("↓", "Move down", form::can_move(items, i, 1).then(|| Msg::Move(ip.clone(), 1))))
                .push(icon("✕", "Delete", Some(Msg::Delete(ip.clone()))))
                .into()
        };
        match it {
            // Blank lines are kept in the file but not shown.
            Item::Blank => Space::new().height(0).into(),
            Item::Comment { text: t } if form::is_disabled_directive(t) => {
                let shown = t.trim().trim_end_matches(';').to_string();
                container(
                    row![
                        text(shown).font(Font::MONOSPACE).size(13).color(c.muted).width(Length::Fill),
                        pill("disabled", style::pill_muted),
                        button(text("Enable").size(13)).padding([5, 12]).style(style::secondary).on_press(Msg::Enable(ip.clone())),
                        tools(vec![]),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding([8, 14])
                .style(style::card)
                .into()
            }
            Item::Comment { text: t } => {
                let line: Element<'_, Msg> = if self.editing.as_ref() == Some(&ip) {
                    let ip2 = ip.clone();
                    row![
                        text("#").color(c.muted),
                        text_input("comment", t)
                            .id(COMMENT)
                            .on_input(move |v| Msg::CommentText(ip2.clone(), v))
                            .on_submit(Msg::EditComment(None))
                            .size(13)
                            .padding([5, 8])
                            .style(style::input(false))
                            .width(Length::Fill),
                        button(text("Done").size(13)).padding([5, 12]).style(style::secondary).on_press(Msg::EditComment(None)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .width(Length::Fill)
                    .into()
                } else {
                    text(format!("# {}", t.trim())).font(style::ITALIC).size(13).color(c.muted).width(Length::Fill).into()
                };
                // Comments get fewer tools, to keep them quiet; the text
                // view can move them.
                let actions = row![
                    icon("✎", "Edit comment", Some(Msg::EditComment(Some(ip.clone())))),
                    icon("✕", "Delete", Some(Msg::Delete(ip.clone()))),
                ];
                container(row![line, actions].spacing(8).align_y(Alignment::Center)).padding([0, 14]).into()
            }
            Item::Dir { name, args, block: Some(b), .. } => {
                let n = form::directive_count(b);
                let inner = form::inner_ctx(name, ctx);
                let summary = format!("{} · {n} directive{}", form::ctx_label(inner), if n == 1 { "" } else { "s" });
                container(
                    row![
                        column![
                            row![text(name.as_str()).font(style::SEMIBOLD), text(args.join(" ")).color(c.muted)].spacing(8),
                            text(summary).size(12).color(c.muted),
                        ]
                        .push(self.messages(&ip))
                        .spacing(2)
                        .width(Length::Fill),
                        button(text("Open  →").size(13)).padding([6, 14]).style(style::secondary).on_press(Msg::Select(ip.clone())),
                        tools(vec![]),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding([12, 14])
                .style(style::card)
                .into()
            }
            Item::Dir { name, args, .. } => {
                let spec = form::spec_for(&self.specs, name, ctx);
                let unknown = form::unknown(ctx);
                let default = spec.and_then(help::default_args);
                let is_default = default.as_deref() == Some(args.as_slice());
                let sub: Element<'_, Msg> = match spec {
                    Some(s) => row![text(s.module).size(12).color(c.muted)]
                        .push(is_default.then(|| {
                            with_tip(pill("default", style::pill_muted), "The same as the default value: this line could be removed.")
                        }))
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .into(),
                    None => {
                        let badge = pill(unknown.label, if unknown.warn { style::pill_warn } else { style::pill_muted });
                        row![with_tip(badge, unknown.help)].into()
                    }
                };
                let label = column![text(name.as_str()).font(style::SEMIBOLD), sub].spacing(4).width(190);
                let body: Element<'_, Msg> = match spec {
                    Some(s) => {
                        let mut note = if s.default.is_empty() { s.help.to_string() } else { format!("{} Default: {}.", s.help, s.default) };
                        if let Some((v, from)) = help::overrides(&self.tree, &self.sel, name) {
                            note.push_str(&format!(" Replaces {} set in {from}.", form::join_args(&v)));
                        }
                        column![self.fields(ip.clone(), s, args), text(note).size(12).color(c.muted)]
                            .push(self.messages(&ip))
                            .spacing(6)
                            .into()
                    }
                    None => column![self.generic(ip.clone(), it), text(unknown.help).size(12).color(c.muted)].spacing(6).into(),
                };
                let mut extra = Vec::new();
                if let Some(d) = default.filter(|_| !is_default) {
                    extra.push(icon("↺", "Reset to the default value", Some(Msg::SetArgs(ip.clone(), d))));
                }
                extra.push(icon("⊘", "Disable (comment out)", Some(Msg::Disable(ip.clone()))));
                container(
                    row![label, container(body).width(Length::Fill), tools(extra)]
                        .spacing(14)
                        .align_y(Alignment::Center),
                )
                .padding([12, 14])
                .style(style::card)
                .into()
            }
        }
    }

    /// The check's messages about the directive at `ip`.
    fn messages(&self, ip: &[usize]) -> Option<Element<'_, Msg>> {
        let mut col = column![].spacing(2);
        let mut any = false;
        for (_, tone, m) in self.located.iter().filter(|(p, _, _)| p == ip) {
            any = true;
            col = col.push(
                row![text("●").size(10).color(style::tone(*tone)), text(m.as_str()).size(12).color(style::tone(*tone))]
                    .spacing(6)
                    .align_y(Alignment::Center),
            );
        }
        any.then(|| col.into())
    }

    /// Notes about the open block: the check's messages about it and how
    /// its settings fit together, with a button that fixes it when there
    /// is one.
    fn notices(&self) -> Option<Element<'_, Msg>> {
        let mut list: Vec<(Tone, String, Option<help::Fix>)> = self
            .located
            .iter()
            .filter(|(p, _, _)| *p == self.sel && !self.sel.is_empty())
            .map(|(_, t, m)| (*t, m.clone(), None))
            .collect();
        list.extend(help::hints(&self.tree, &self.sel).into_iter().map(|h| (h.tone, h.text, h.fix)));
        if list.is_empty() {
            return None;
        }
        let mut col = column![].spacing(8);
        for (t, m, fix) in list {
            let mark = match t {
                Tone::Info => "i",
                Tone::Warn | Tone::Err => "!",
            };
            let badge = container(text(mark).size(11).font(style::SEMIBOLD).color(style::tone(t)))
                .center_x(18)
                .center_y(18)
                .style(move |_: &Theme| container::Style {
                    border: Border { radius: 9.0.into(), width: 1.5, color: style::tone(t) },
                    ..Default::default()
                });
            let fix = fix.map(|f| button(text(f.label).size(13)).padding([5, 12]).style(style::secondary).on_press(Msg::Fix(f.add)));
            col = col.push(
                container(row![badge, text(m).size(13).width(Length::Fill)].push(fix).spacing(10).align_y(Alignment::Center))
                    .padding([9, 12])
                    .style(style::notice(t)),
            );
        }
        Some(col.into())
    }

    /// Settings of outer blocks that apply in the open block.
    fn inherited_card(&self) -> Option<Element<'_, Msg>> {
        let c = style::colors();
        let list = help::inherited(&self.specs, &self.tree, &self.sel);
        if list.is_empty() {
            return None;
        }
        let mut rows = column![].spacing(4);
        for h in list {
            let override_ = button(text("Override").size(12)).padding([4, 10]).style(style::secondary);
            rows = rows.push(
                row![
                    text(h.name.clone()).font(Font { weight: iced::font::Weight::Semibold, ..Font::MONOSPACE }).size(13).width(190),
                    text(form::join_args(&h.args)).font(Font::MONOSPACE).size(13).width(Length::Fill),
                    text(format!("from {}", h.from)).size(12).color(c.muted),
                    with_tip(override_.on_press(Msg::Override(h.name, h.args)), "Give this block its own value"),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        Some(
            container(
                column![
                    row![
                        text("Inherited").font(style::SEMIBOLD),
                        text("Set in outer blocks; they apply here too.").size(12).color(c.muted),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                    rows,
                ]
                .spacing(10),
            )
            .padding([14, 16])
            .style(style::card)
            .into(),
        )
    }

    /// The plain "arguments" field of a directive this server doesn't know.
    fn generic(&self, ip: ItemPath, it: &Item) -> Element<'_, Msg> {
        let Item::Dir { args, .. } = it else { return text("").into() };
        let value = self.drafts.get(&(ip.clone(), GENERIC)).cloned().unwrap_or_else(|| form::join_args(args));
        field_input("arguments", &value, false).on_input(move |v| Msg::Generic(ip.clone(), v)).width(360).into()
    }

    /// The fields of directive `ip` described by `spec`.
    fn fields(&self, ip: ItemPath, spec: &Spec, args: &[String]) -> Element<'_, Msg> {
        let c = style::colors();
        let vals = form::assign(spec, args);
        let mut out = row![].spacing(16).align_y(Alignment::Center);
        for (n, (a, v)) in spec.args.iter().zip(&vals).enumerate() {
            let label = text(format!("{}{}", a.name, if a.opt { " (optional)" } else { "" })).size(12).color(c.muted);
            let widget = self.field(ip.clone(), n, spec, a, v);
            out = out.push(row![label, widget].spacing(8).align_y(Alignment::Center));
        }
        out.wrap().vertical_spacing(8).into()
    }

    fn field(&self, ip: ItemPath, n: usize, spec: &Spec, a: &Arg, v: &Val) -> Element<'_, Msg> {
        let c = style::colors();
        let list_field = |ip: ItemPath, v: &Val, width: f32| -> Element<'_, Msg> {
            let value = self.drafts.get(&(ip.clone(), n)).cloned().unwrap_or_else(|| form::join_args(&v.as_list()));
            field_input(a.name, &value, false).on_input(move |s| Msg::SetList(ip.clone(), n, s)).width(width).into()
        };
        match (&a.kind, v) {
            (Kind::Choices(known), Val::Many(cur)) => {
                let mut boxes = row![].spacing(14).align_y(Alignment::Center);
                for b in form::choice_boxes(known, cur) {
                    match b {
                        form::ChoiceBox::Plain(o) => {
                            let on = cur.contains(&o);
                            let ip = ip.clone();
                            boxes = boxes.push(
                                checkbox(on).label(o.clone()).size(16).text_size(13).on_toggle(move |b| Msg::Toggle(ip.clone(), n, o.clone(), b)),
                            );
                        }
                        form::ChoiceBox::Switch { key, values } => {
                            let set = cur.iter().find_map(|c| c.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.to_string()));
                            let (ip1, ip2) = (ip.clone(), ip.clone());
                            let k2 = key.clone();
                            let first = format!("{key}={}", values[0]);
                            let now = set.as_ref().map(|v| format!("{key}={v}")).unwrap_or_else(|| first.clone());
                            let mut r = row![checkbox(set.is_some())
                                .label(key)
                                .size(16)
                                .text_size(13)
                                .on_toggle(move |b| Msg::Toggle(ip1.clone(), n, if b { first.clone() } else { now.clone() }, b))]
                            .spacing(6)
                            .align_y(Alignment::Center);
                            if let Some(v) = set {
                                r = r.push(select(values, Some(v), move |v| Msg::Toggle(ip2.clone(), n, format!("{k2}={v}"), true)));
                            }
                            boxes = boxes.push(r);
                        }
                    }
                }
                boxes.wrap().vertical_spacing(6).into()
            }
            (Kind::Param(keys), Val::Many(cur)) => {
                let mut list = row![].spacing(10).align_y(Alignment::Center);
                for (j, kv) in cur.iter().enumerate() {
                    let (key, value) = form::split_param(kv);
                    let mut options: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
                    if !keys.contains(&key) {
                        options.insert(0, key.to_string());
                    }
                    let (ip1, ip2, ip3) = (ip.clone(), ip.clone(), ip.clone());
                    list = list.push(
                        row![
                            select(options, Some(key.to_string()), move |k| Msg::ParamKey(ip1.clone(), n, j, k)),
                            text("="),
                            field_input("value", value, false)
                                .on_input(move |s| Msg::ParamValue(ip2.clone(), n, j, s))
                                .width(130),
                            icon("✕", "Remove", Some(Msg::ParamRemove(ip3, n, j))),
                        ]
                        .spacing(4)
                        .align_y(Alignment::Center),
                    );
                }
                list = list.push(
                    button(text("+ parameter").size(13)).padding([5, 12]).style(style::secondary).on_press(Msg::ParamAdd(ip, n)),
                );
                list.wrap().vertical_spacing(8).into()
            }
            (_, Val::Many(_)) => list_field(ip, v, 360.0),
            (Kind::Flag | Kind::Choice(_), Val::One(cur)) => {
                let mut options: Vec<String> = match a.kind {
                    Kind::Choice(vs) => vs.iter().map(|s| s.to_string()).collect(),
                    _ => vec!["on".into(), "off".into()],
                };
                if !cur.is_empty() && !options.contains(cur) {
                    options.insert(0, cur.clone());
                }
                if a.opt {
                    options.insert(0, NONE.into());
                }
                let selected = if cur.is_empty() { a.opt.then(|| NONE.to_string()) } else { Some(cur.clone()) };
                select(options, selected, move |s: String| Msg::SetOne(ip.clone(), n, if s == NONE { String::new() } else { s }))
            }
            (kind, Val::One(cur)) => {
                let problem = help::field_problem(spec, a, cur);
                let wide = matches!(kind, Kind::Url | Kind::Path | Kind::Text);
                let input = field_input(form::placeholder(a), cur, problem.is_some())
                    .on_input(move |s| Msg::SetOne(ip.clone(), n, s))
                    .width(if wide { 360 } else { 140 });
                match problem {
                    Some(p) => column![input, text(p).size(11).color(c.danger)].spacing(3).into(),
                    None => input.into(),
                }
            }
        }
    }

    fn add_bar(&self) -> Element<'_, Msg> {
        let c = style::colors();
        let groups = self.matches();
        let q = self.query.trim();
        let written = (form::parse_custom(&self.query).is_some() && (groups.is_empty() || q.contains(char::is_whitespace)))
            .then(|| {
                button(
                    row![
                        text("Add as written:").size(13).color(c.muted),
                        text(format!("{};", q.trim_end_matches(';'))).font(Font::MONOSPACE).size(13),
                        Space::new().width(Length::Fill),
                        text("Enter").size(11).color(c.muted),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([8, 10])
                .style(style::entry)
                .on_press(Msg::AddSubmit)
            });
        let mut list = column![].spacing(2).push(written);
        for (module, specs) in &groups {
            list = list.push(
                container(text(module.to_uppercase()).size(11).font(style::SEMIBOLD).color(c.muted)).padding(
                    iced::Padding { top: 8.0, bottom: 2.0, left: 10.0, right: 10.0 },
                ),
            );
            for s in specs {
                let name = row![text(s.name).font(Font { weight: iced::font::Weight::Semibold, ..Font::MONOSPACE }).size(13)]
                    .push(s.block.is_some().then(|| text(" { }").font(Font::MONOSPACE).size(13).color(c.muted)));
                let help = container(text(s.help).size(12).color(c.muted).wrapping(text::Wrapping::None))
                    .width(Length::Fill)
                    .clip(true);
                list = list.push(
                    button(row![container(name).width(220), help].spacing(12).align_y(Alignment::Center))
                        .width(Length::Fill)
                        .padding([6, 10])
                        .style(style::entry)
                        .on_press(Msg::Add(s.name)),
                );
            }
        }
        if groups.is_empty() && !q.contains(char::is_whitespace) {
            let msg = if q.is_empty() {
                "Every directive this block accepts is already here.".to_string()
            } else {
                format!("No known directive matches \"{q}\".")
            };
            list = list.push(container(text(msg).size(13).color(c.muted)).padding([10, 10]));
        }
        let count: usize = groups.iter().map(|(_, l)| l.len()).sum();
        let hint = if q.is_empty() {
            format!("{count} directives available. Click one to add it with its default values.")
        } else {
            format!("{count} matching. Enter adds the exact or only match; with arguments typed, the text as written.")
        };
        container(
            column![
                row![
                    text("Add to this block").font(style::SEMIBOLD),
                    Space::new().width(Length::Fill),
                    button(text("Add comment").size(13)).padding([6, 12]).style(style::secondary).on_press(Msg::AddComment),
                ]
                .align_y(Alignment::Center),
            ]
            .push(self.recipe_bar())
            .push(
                field_input("Search directives, or type name and arguments", &self.query, false)
                    .id(ADD)
                    .on_input(Msg::AddQuery)
                    .on_submit(Msg::AddSubmit)
                    .padding([8, 12])
                    .width(Length::Fill),
            )
            .push(container(scrollable(list).height(Length::Shrink)).max_height(300).padding(4).style(style::well))
            .push(text(hint).size(12).color(c.muted))
            .spacing(10),
        )
        .padding([14, 16])
        .style(style::card)
        .into()
    }

    /// Buttons that add a ready-made snippet for a common setup.
    fn recipe_bar(&self) -> Option<Element<'_, Msg>> {
        let c = style::colors();
        let items = form::items_at(&self.tree, &self.sel)?;
        let list = help::recipes(form::ctx_of(&self.tree, &self.sel), items);
        if list.is_empty() {
            return None;
        }
        let mut buttons = row![].spacing(8);
        for k in list {
            let r = &help::RECIPES[k];
            buttons = buttons.push(with_tip(
                button(text(r.name).size(13)).padding([6, 12]).style(style::secondary).on_press(Msg::Recipe(k)),
                r.help,
            ));
        }
        Some(
            column![text("Quick start").size(12).font(style::SEMIBOLD).color(c.muted), buttons.wrap().vertical_spacing(8)]
                .spacing(6)
                .into(),
        )
    }

    fn dialog_view(&self, d: &Dialog) -> Element<'_, Msg> {
        let (question, yes, no) = match d {
            Dialog::DeleteBlock(ip) => {
                let what = match self.item(ip) {
                    Some(Item::Dir { name, args, .. }) => format!("{name} {}", args.join(" ")).trim().to_string(),
                    _ => "this block".into(),
                };
                (format!("Delete \"{what}\" and everything in it?"), "Delete", "Cancel")
            }
            Dialog::Overwrite => (
                "nginx.conf was changed by another program since it was loaded. Overwrite it?".into(),
                "Overwrite",
                "Cancel",
            ),
            Dialog::Discard => ("Discard your changes?".into(), "Discard", "Cancel"),
            Dialog::ReloadUnsaved => (
                "Your changes are not saved; reload the server with the file as it is on disk?".into(),
                "Reload",
                "Cancel",
            ),
            Dialog::Close => ("The changes are not saved. Close the editor anyway?".into(), "Close", "Keep editing"),
        };
        let card = container(
            column![
                text(question).size(15),
                row![
                    Space::new().width(Length::Fill),
                    button(text(no).size(13)).padding([7, 14]).style(style::secondary).on_press(Msg::Answer(false)),
                    button(text(yes).size(13).font(style::SEMIBOLD))
                        .padding([7, 16])
                        .style(style::danger)
                        .on_press(Msg::Answer(true)),
                ]
                .spacing(8),
            ]
            .spacing(22),
        )
        .padding(22)
        .max_width(460)
        .style(style::dialog);
        opaque(container(card).center(Length::Fill).style(style::backdrop))
    }
}

/// The "no value" entry of an optional choice.
const NONE: &str = "—";

/// The scrollable form, scrolled back to the top when another block is opened.
const PANE: &str = "pane";
/// The field of the comment being edited, focused when editing starts.
const COMMENT: &str = "comment";
/// The search box of the "add" card.
const ADD: &str = "add";

fn split(ip: &[usize]) -> (&[usize], usize) {
    match ip.split_last() {
        Some((i, parent)) => (parent, *i),
        None => (&[], usize::MAX),
    }
}

fn hline<'a>() -> Element<'a, Msg> {
    rule::horizontal(1).style(|t| rule::Style { color: style::colors().border, ..rule::default(t) }).into()
}

fn vline<'a>() -> Element<'a, Msg> {
    rule::vertical(1).style(|t| rule::Style { color: style::colors().border, ..rule::default(t) }).into()
}

fn pill<'a>(s: impl Into<String>, st: fn(&Theme) -> container::Style) -> Element<'a, Msg> {
    container(text(s.into()).size(11).font(style::SEMIBOLD).wrapping(text::Wrapping::None)).padding([2, 9]).style(st).into()
}

fn tip<'a>(s: &'a str) -> Element<'a, Msg> {
    container(text(s).size(12)).padding([6, 10]).max_width(380).style(style::tooltip).into()
}

fn with_tip<'a>(content: impl Into<Element<'a, Msg>>, s: &'a str) -> Element<'a, Msg> {
    tooltip(content, tip(s), tooltip::Position::Bottom).gap(4).into()
}

fn icon<'a>(label: &'a str, hint: &'a str, msg: Option<Msg>) -> Element<'a, Msg> {
    tooltip(
        button(text(label).size(14)).padding([3, 8]).style(style::ghost).on_press_maybe(msg),
        tip(hint),
        tooltip::Position::Top,
    )
    .gap(2)
    .into()
}

fn field_input<'a>(placeholder: &str, value: &str, bad: bool) -> iced::widget::TextInput<'a, Msg> {
    text_input(placeholder, value).size(13).padding([6, 10]).style(style::input(bad))
}

fn select<'a>(options: Vec<String>, selected: Option<String>, f: impl Fn(String) -> Msg + 'a) -> Element<'a, Msg> {
    pick_list(options, selected, f).text_size(13).padding([6, 10]).style(style::select).into()
}
