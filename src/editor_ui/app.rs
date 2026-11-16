//! The editor window: the configuration as a tree of blocks with a form
//! for each directive, or as text. The same editor as the web page
//! (`editor.html`), drawn with iced.

use super::api::{Api, Content, Loaded, Saved};
use iced::widget::{
    Space, button, checkbox, column, container, opaque, pick_list, row, rule, scrollable, stack, text, text_editor,
    text_input, tooltip,
};
use iced::{Alignment, Border, Color, Element, Font, Length, Subscription, Task, Theme, event, keyboard, window};
use nginx_rtmp::conf_edit::{self, Item};
use nginx_rtmp::conf_form::{self as form, Check, Level, Val};
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
    // rows
    Move(ItemPath, isize),
    Delete(ItemPath),
    Disable(ItemPath),
    Enable(ItemPath),
    // adding
    AddPick(AddOption),
    Add,
    AddComment,
    Custom(String),
    CustomSubmit,
    // text view
    Edit(text_editor::Action),
    // header
    Save,
    Revert,
    ReloadServer,
    Answer(bool),
}

/// An entry of the "add a directive" list.
#[derive(Debug, Clone, PartialEq)]
struct AddOption {
    name: &'static str,
    label: String,
}

impl std::fmt::Display for AddOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
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
    status: Vec<(Level, String)>,
    /// Shown first after the next load (the outcome of a save).
    pending: Option<(Level, String)>,
    busy: bool,
    dialog: Option<Dialog>,
    add_pick: Option<AddOption>,
    custom: String,
    /// Text typed in fields that hold several arguments, kept as typed
    /// (re-joining the arguments would eat a trailing space).
    drafts: HashMap<(ItemPath, usize), String>,
}

/// Field number of the plain "arguments" field of unknown directives.
const GENERIC: usize = usize::MAX;

pub fn run(api: Api) -> iced::Result {
    iced::application(
        move || {
            let e = Editor::new(api.clone());
            let load = e.load();
            (e, load)
        },
        Editor::update,
        Editor::view,
    )
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
            status: vec![(Level::Ok, "Loading…".into())],
            pending: None,
            busy: false,
            dialog: None,
            add_pick: None,
            custom: String::new(),
            drafts: HashMap::new(),
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
                self.drafts.clear();
                self.add_pick = None;
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
                cur.retain(|v| v != &value);
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
            Msg::AddPick(o) => self.add_pick = Some(o),
            Msg::Add => {
                let ctx = form::ctx_of(&self.tree, &self.sel);
                let Some(name) = self.add_pick.as_ref().map(|o| o.name) else { return Task::none() };
                let Some(spec) = form::spec_for(&self.specs, name, ctx) else { return Task::none() };
                let Some(items) = form::items_at_mut(&mut self.tree, &self.sel) else { return Task::none() };
                let (at, open) = form::add(items, spec);
                self.add_pick = None;
                self.structure_changed(self.sel.clone());
                if open {
                    self.sel.push(at);
                }
            }
            Msg::AddComment => self.push_item(Item::Comment { text: " ".into() }),
            Msg::Custom(v) => self.custom = v,
            Msg::CustomSubmit => {
                if let Some(it) = form::parse_custom(&self.custom) {
                    self.custom.clear();
                    self.push_item(it);
                }
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
    }

    /// Status lines: `extra` first, then the check (its "valid" line only
    /// without `extra`).
    fn show(&mut self, extra: Option<(Level, String)>) {
        let mut out: Vec<(Level, String)> = extra.clone().into_iter().collect();
        if let Some(c) = &self.check {
            out.extend(form::check_messages(c, self.dirty).into_iter().filter(|(l, _)| extra.is_none() || *l != Level::Ok));
        }
        self.status = out;
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

    // ------------------------------------------------------------- view

    fn view(&self) -> Element<'_, Msg> {
        let body: Element<'_, Msg> = match self.mode {
            Mode::Form => row![
                container(scrollable(self.tree_view()).height(Length::Fill)).width(270).height(Length::Fill),
                rule::vertical(1),
                scrollable(container(self.pane()).padding([16, 20])).width(Length::Fill).height(Length::Fill),
            ]
            .height(Length::Fill)
            .into(),
            Mode::Text => container(
                text_editor(&self.content)
                    .on_action(Msg::Edit)
                    .font(Font::MONOSPACE)
                    .height(Length::Fill)
                    .padding(10),
            )
            .padding([12, 16])
            .height(Length::Fill)
            .into(),
        };
        let page = column![self.header(), rule::horizontal(1), body, rule::horizontal(1), self.footer()];
        // Always a stack, so the page keeps its place (and its scroll
        // position) when a dialog opens over it.
        let mut layers = stack![page];
        if let Some(d) = &self.dialog {
            layers = layers.push(self.dialog_view(d));
        }
        layers.into()
    }

    fn header(&self) -> Element<'_, Msg> {
        let tab = |label, m: Mode| {
            button(text(label))
                .style(if self.mode == m { button::primary } else { button::secondary })
                .on_press(Msg::SetMode(m))
        };
        let title = if self.dirty { "nginx.conf •" } else { "nginx.conf" };
        // A long path is cut off rather than pushing the buttons out.
        let name = column![
            text(title).size(20),
            text(&self.file).size(12).style(text::secondary).wrapping(text::Wrapping::None),
        ];
        row![
            container(name).width(Length::Fill).clip(true),
            row![tab("Form", Mode::Form), tab("Text", Mode::Text)].spacing(2),
            Space::new().width(16),
            tooltip(
                button(text("Revert")).style(button::secondary).on_press_maybe(self.loaded.then_some(Msg::Revert)),
                hint("Discard changes and load the file again"),
                tooltip::Position::Bottom,
            ),
            tooltip(
                button(text("Reload server")).style(button::secondary).on_press(Msg::ReloadServer),
                hint("Reload the file as it is on disk into the running server"),
                tooltip::Position::Bottom,
            ),
            tooltip(
                button(text(if self.busy { "Saving…" } else { "Save & reload" }))
                    .style(button::primary)
                    .on_press_maybe((!self.busy && self.loaded).then_some(Msg::Save)),
                hint("Check, save and reload the running server (Ctrl+S)"),
                tooltip::Position::Bottom,
            ),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .padding([10, 16])
        .into()
    }

    fn footer(&self) -> Element<'_, Msg> {
        let lines = self.status.iter().map(|(l, m)| {
            let t = text(m.as_str()).size(13);
            match l {
                Level::Ok => t.style(text::success),
                Level::Warn => t.style(text::warning),
                Level::Err => t.style(text::danger),
            }
            .into()
        });
        container(scrollable(column(lines).spacing(2)).height(Length::Shrink))
            .max_height(120)
            .padding([8, 16])
            .width(Length::Fill)
            .into()
    }

    fn tree_view(&self) -> Element<'_, Msg> {
        let node = |depth: usize, name: String, detail: String, path: Vec<usize>| -> Element<'_, Msg> {
            let on = path == self.sel;
            let detail = text(detail).size(13).wrapping(text::Wrapping::None);
            let detail = if on {
                detail.style(|t: &Theme| text::Style { color: Some(t.extended_palette().primary.base.text.scale_alpha(0.75)) })
            } else {
                detail.style(text::secondary)
            };
            row![
                Space::new().width((depth * 14) as f32),
                button(row![text(name).wrapping(text::Wrapping::None), detail].spacing(6))
                    .width(Length::Fill)
                    .style(if on { button::primary } else { button::text })
                    .on_press(Msg::Select(path)),
            ]
            .into()
        };
        let mut col = column![node(0, "nginx.conf".into(), "main".into(), Vec::new())].spacing(1).padding(8);
        for n in form::blocks(&self.tree) {
            col = col.push(node(n.depth, n.name, n.detail, n.path));
        }
        col.into()
    }

    fn pane(&self) -> Element<'_, Msg> {
        let tree = &self.tree;
        let ctx = form::ctx_of(tree, &self.sel);
        let Some(items) = form::items_at(tree, &self.sel) else { return text("").into() };
        let me = form::item_at(tree, &self.sel);
        let mut col = column![
            text(form::crumbs(tree, &self.sel).join(" › ")).size(13).style(text::secondary),
            row![
                text(match me {
                    Some(Item::Dir { name, .. }) => name.as_str(),
                    _ => "nginx.conf",
                })
                .size(22),
                badge(form::ctx_label(ctx).to_string()),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        match me {
            Some(it @ Item::Dir { name, args, .. }) => {
                let parent_ctx = form::ctx_of(tree, &self.sel[..self.sel.len() - 1]);
                match form::spec_for(&self.specs, name, parent_ctx) {
                    Some(spec) => {
                        col = col.push(text(spec.help).style(text::secondary));
                        if !spec.args.is_empty() {
                            col = col.push(self.fields(self.sel.clone(), spec, args));
                        }
                    }
                    None if !args.is_empty() => col = col.push(self.generic(self.sel.clone(), it)),
                    None => {}
                }
            }
            _ => {
                col = col.push(
                    text("Top level of the file. Open a block on the left, or add one below.").style(text::secondary),
                )
            }
        }
        let mut rows = column![].spacing(0);
        for (i, it) in items.iter().enumerate() {
            let mut ip = self.sel.clone();
            ip.push(i);
            rows = rows.push(self.item_row(ip, it, i, items.len(), ctx));
            rows = rows.push(rule::horizontal(1));
        }
        if items.is_empty() {
            rows = rows.push(text("Empty block.").style(text::secondary));
        }
        col = col.push(rows).push(self.add_bar(items, ctx));
        if let Some(Item::Dir { name, .. }) = me {
            col = col.push(
                button(text(format!("Delete this {name} block")))
                    .style(button::danger)
                    .on_press(Msg::Delete(self.sel.clone())),
            );
        }
        col.into()
    }

    fn item_row(&self, ip: ItemPath, it: &Item, i: usize, len: usize, ctx: &str) -> Element<'_, Msg> {
        let (name_cell, body, extra): (Element<'_, Msg>, Element<'_, Msg>, Option<Element<'_, Msg>>) = match it {
            Item::Blank => (text("").into(), text("Empty line").size(13).style(text::secondary).into(), None),
            Item::Comment { text: t } => {
                let disabled = form::is_disabled_directive(t);
                let input = text_input("comment", t).on_input({
                    let ip = ip.clone();
                    move |v| Msg::CommentText(ip.clone(), v)
                });
                let enable = disabled.then(|| tool("✓", "Enable this directive", Some(Msg::Enable(ip.clone()))));
                (name_with("#", if disabled { "disabled directive" } else { "comment" }), input.into(), enable)
            }
            Item::Dir { name, args, block: Some(b), .. } => {
                let n = form::directive_count(b);
                let inner = form::inner_ctx(name, ctx);
                let label = format!("{name} {}", args.join(" "));
                let summary = row![
                    text(format!("{n} directive{}", if n == 1 { "" } else { "s" })),
                    button(text("Open")).style(button::secondary).on_press(Msg::Select(ip.clone())),
                ]
                .spacing(10)
                .align_y(Alignment::Center);
                (name_with_owned(label.trim().to_string(), form::ctx_label(inner).to_string()), summary.into(), None)
            }
            Item::Dir { name, args, .. } => {
                let spec = form::spec_for(&self.specs, name, ctx);
                let cell = match spec {
                    Some(s) => {
                        let tip = if s.default.is_empty() { s.help.to_string() } else { format!("{}\nDefault: {}", s.help, s.default) };
                        tooltip(name_with_owned(name.clone(), s.module.to_string()), hint_owned(tip), tooltip::Position::Bottom)
                            .into()
                    }
                    None => name_with_owned(name.clone(), "unknown".into()),
                };
                let body: Element<'_, Msg> = match spec {
                    Some(s) => {
                        let note = if s.default.is_empty() { s.help.to_string() } else { format!("{} Default: {}.", s.help, s.default) };
                        column![self.fields(ip.clone(), s, args), text(note).size(12).style(text::secondary)].spacing(4).into()
                    }
                    None => row![self.generic(ip.clone(), it), badge("not used by this server".into())]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into(),
                };
                (cell, body, Some(tool("#", "Disable (comment out)", Some(Msg::Disable(ip.clone())))))
            }
        };
        let tools = row![
            extra.unwrap_or_else(|| Space::new().into()),
            tool("↑", "Move up", (i > 0).then(|| Msg::Move(ip.clone(), -1))),
            tool("↓", "Move down", (i + 1 < len).then(|| Msg::Move(ip.clone(), 1))),
            tool("✕", "Delete", Some(Msg::Delete(ip.clone()))),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        row![container(name_cell).width(210), container(body).width(Length::Fill), tools]
            .spacing(12)
            .padding([8, 0])
            .align_y(Alignment::Center)
            .into()
    }

    /// The plain "arguments" field of a directive this server doesn't know.
    fn generic(&self, ip: ItemPath, it: &Item) -> Element<'_, Msg> {
        let Item::Dir { args, .. } = it else { return text("").into() };
        let value = self.drafts.get(&(ip.clone(), GENERIC)).cloned().unwrap_or_else(|| form::join_args(args));
        text_input("arguments", &value).on_input(move |v| Msg::Generic(ip.clone(), v)).width(360).into()
    }

    /// The fields of directive `ip` described by `spec`.
    fn fields(&self, ip: ItemPath, spec: &Spec, args: &[String]) -> Element<'_, Msg> {
        let vals = form::assign(spec, args);
        let mut out = row![].spacing(14).align_y(Alignment::Center);
        for (n, (a, v)) in spec.args.iter().zip(&vals).enumerate() {
            let label = text(format!("{}{}", a.name, if a.opt { "?" } else { "" })).size(13).style(text::secondary);
            let widget = self.field(ip.clone(), n, a, v);
            out = out.push(row![label, widget].spacing(6).align_y(Alignment::Center));
        }
        out.wrap().vertical_spacing(6).into()
    }

    fn field(&self, ip: ItemPath, n: usize, a: &Arg, v: &Val) -> Element<'_, Msg> {
        let list_field = |ip: ItemPath, v: &Val, width: f32| -> Element<'_, Msg> {
            let value = self.drafts.get(&(ip.clone(), n)).cloned().unwrap_or_else(|| form::join_args(&v.as_list()));
            text_input(a.name, &value).on_input(move |s| Msg::SetList(ip.clone(), n, s)).width(width).into()
        };
        match (&a.kind, v) {
            (Kind::Choices(known), Val::Many(cur)) => {
                let mut boxes = row![].spacing(10);
                let extra = cur.iter().filter(|c| !known.contains(&c.as_str())).cloned();
                for o in known.iter().map(|s| s.to_string()).chain(extra) {
                    let on = cur.contains(&o);
                    let ip = ip.clone();
                    let value = o.clone();
                    boxes = boxes.push(checkbox(on).label(o).on_toggle(move |b| Msg::Toggle(ip.clone(), n, value.clone(), b)));
                }
                boxes.wrap().into()
            }
            (Kind::Param(keys), Val::Many(cur)) => {
                let mut list = row![].spacing(8).align_y(Alignment::Center);
                for (j, kv) in cur.iter().enumerate() {
                    let (key, value) = form::split_param(kv);
                    let mut options: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
                    if !keys.contains(&key) {
                        options.insert(0, key.to_string());
                    }
                    let (ip1, ip2, ip3) = (ip.clone(), ip.clone(), ip.clone());
                    list = list.push(
                        row![
                            pick_list(options, Some(key.to_string()), move |k| Msg::ParamKey(ip1.clone(), n, j, k)),
                            text("="),
                            text_input("value", value).on_input(move |s| Msg::ParamValue(ip2.clone(), n, j, s)).width(120),
                            tool("✕", "Remove", Some(Msg::ParamRemove(ip3, n, j))),
                        ]
                        .spacing(4)
                        .align_y(Alignment::Center),
                    );
                }
                list = list.push(button(text("+ param").size(13)).style(button::secondary).on_press(Msg::ParamAdd(ip, n)));
                list.wrap().vertical_spacing(6).into()
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
                pick_list(options, selected, move |s: String| Msg::SetOne(ip.clone(), n, if s == NONE { String::new() } else { s }))
                    .placeholder(a.name)
                    .into()
            }
            (kind, Val::One(cur)) => {
                let bad = !cur.is_empty() && !form::valid(kind, cur);
                let wide = matches!(kind, Kind::Url | Kind::Path | Kind::Text);
                text_input(form::placeholder(a), cur)
                    .on_input(move |s| Msg::SetOne(ip.clone(), n, s))
                    .width(if wide { 360 } else { 150 })
                    .style(move |t: &Theme, s| {
                        let mut st = text_input::default(t, s);
                        if bad {
                            st.border.color = t.palette().danger;
                        }
                        st
                    })
                    .into()
            }
        }
    }

    fn add_bar<'a>(&'a self, items: &[Item], ctx: &str) -> Element<'a, Msg> {
        let options: Vec<AddOption> = form::available(&self.specs, items, ctx)
            .into_iter()
            .flat_map(|(module, list)| {
                list.into_iter().map(move |s| AddOption {
                    name: s.name,
                    label: format!("{module}: {}{}", s.name, if s.block.is_some() { " { }" } else { "" }),
                })
            })
            .collect();
        let help = self
            .add_pick
            .as_ref()
            .and_then(|o| form::spec_for(&self.specs, o.name, ctx))
            .map(|s| if s.default.is_empty() { s.help.to_string() } else { format!("{} Default: {}.", s.help, s.default) })
            .unwrap_or_default();
        column![
            row![
                pick_list(options, self.add_pick.clone(), Msg::AddPick).placeholder("Add a directive…").width(320),
                button(text("Add")).style(button::secondary).on_press_maybe(self.add_pick.is_some().then_some(Msg::Add)),
                button(text("Add comment")).style(button::secondary).on_press(Msg::AddComment),
                text_input("or type: name args", &self.custom)
                    .on_input(Msg::Custom)
                    .on_submit(Msg::CustomSubmit)
                    .width(220),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(help).size(13).style(text::secondary),
        ]
        .spacing(6)
        .padding([12, 0])
        .into()
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
                text(question),
                row![
                    Space::new().width(Length::Fill),
                    button(text(no)).style(button::secondary).on_press(Msg::Answer(false)),
                    button(text(yes)).style(button::danger).on_press(Msg::Answer(true)),
                ]
                .spacing(8),
            ]
            .spacing(18),
        )
        .padding(20)
        .max_width(480)
        .style(|t: &Theme| container::Style {
            background: Some(t.palette().background.into()),
            border: Border { radius: 8.0.into(), width: 1.0, color: t.extended_palette().background.strong.color },
            ..container::Style::default()
        });
        opaque(
            container(card)
                .center(Length::Fill)
                .style(|_| container::Style { background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()), ..Default::default() }),
        )
    }
}

/// The "no value" entry of an optional choice.
const NONE: &str = "—";

fn split(ip: &[usize]) -> (&[usize], usize) {
    match ip.split_last() {
        Some((i, parent)) => (parent, *i),
        None => (&[], usize::MAX),
    }
}

fn hint<'a>(s: &'a str) -> Element<'a, Msg> {
    container(text(s).size(13)).padding([4, 8]).style(container::bordered_box).into()
}

fn hint_owned<'a>(s: String) -> Element<'a, Msg> {
    container(text(s).size(13)).padding([4, 8]).max_width(420).style(container::bordered_box).into()
}

fn badge<'a>(s: String) -> Element<'a, Msg> {
    container(text(s).size(12)).padding([2, 8]).style(container::secondary).into()
}

fn name_with<'a>(name: &'a str, small: &'a str) -> Element<'a, Msg> {
    column![text(name), text(small).size(12).style(text::secondary)].into()
}

fn name_with_owned<'a>(name: String, small: String) -> Element<'a, Msg> {
    column![text(name), text(small).size(12).style(text::secondary)].into()
}

fn tool<'a>(label: &'a str, tip: &'a str, msg: Option<Msg>) -> Element<'a, Msg> {
    tooltip(
        button(text(label).size(13)).padding([2, 8]).style(button::secondary).on_press_maybe(msg),
        hint(tip),
        tooltip::Position::Left,
    )
    .into()
}
