use std::{collections::HashMap, path::PathBuf};

use anyhow::Context;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value, json};

use super::schema::{Field, Kind, fields_from_value, parse_input};

#[derive(Debug, Clone, Default)]
pub struct ExtInfo {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub routes: usize,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item {
    Server,
    Tunnel,
    Ext(usize),
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    List,
    Fields,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Edit(String),
    Add(String),
    ConfirmDelete,
    ConfirmQuit,
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Quit,
    Reload,
}

pub struct App {
    pub doc: Value,
    pub path: PathBuf,
    pub dirty: bool,
    pub infos: HashMap<String, ExtInfo>,
    pub problems: HashMap<String, String>,
    pub sel: usize,
    pub field_sel: usize,
    pub focus: Focus,
    pub mode: Mode,
    pub notice: String,
    pub notice_is_error: bool,
    saved_server: Value,
}

fn server_part(doc: &Value) -> Value {
    let mut doc = doc.clone();
    if let Some(map) = doc.as_object_mut() {
        map.remove("extensions");
    }
    doc
}

fn server_fields() -> Vec<Field> {
    vec![
        Field::new(&["port"], Kind::Integer, "Port to listen on (the PORT environment variable overrides it).").with_default(json!(3000)),
        Field::new(&["host"], Kind::Text, "IP address to listen on. 0.0.0.0 = every interface, 127.0.0.1 = this computer only.").with_default(json!("0.0.0.0")),
        Field::new(&["title"], Kind::Text, "Title of the generated API docs.").with_default(json!("samishisa")),
        Field::new(&["accessLog"], Kind::Bool, "One log line per request.").with_default(json!(true)),
        Field::new(&["cors"], Kind::Bool, "Allow browsers on any website to call the API.").with_default(json!(true)),
        Field::new(&["compression"], Kind::Bool, "gzip responses over 1 KB.").with_default(json!(true)),
    ]
}

fn tunnel_fields() -> Vec<Field> {
    vec![
        Field::new(&["tunnel", "enabled"], Kind::Bool, "Run cloudflared alongside the server (needs cloudflared installed).").with_default(json!(false)),
        Field::new(
            &["tunnel", "token"],
            Kind::Text,
            "Token of your named tunnel. Leave empty for a quick tunnel with a random address. Use ${CLOUDFLARE_TUNNEL_TOKEN} to read it from the environment.",
        ),
        Field::new(&["tunnel", "url"], Kind::Text, "Public address of a named tunnel (shown in the log; routing is set in Cloudflare)."),
        Field::new(&["tunnel", "binary"], Kind::Text, "Path of cloudflared, if it isn't on your PATH."),
    ]
}

pub fn source_of(entry: &Value) -> &str {
    entry.as_str().or_else(|| entry.get("source").and_then(Value::as_str)).unwrap_or("?")
}

pub fn enabled_of(entry: &Value) -> bool {
    entry.get("enabled").and_then(Value::as_bool).unwrap_or(true)
}

fn get_path<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(root, |node, key| node.get(key))
}

fn object_mut(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    match value {
        Value::Object(map) => map,
        _ => unreachable!("replaced with an object just above"),
    }
}

fn array_mut(value: &mut Value) -> &mut Vec<Value> {
    if !value.is_array() {
        *value = Value::Array(Vec::new());
    }
    match value {
        Value::Array(list) => list,
        _ => unreachable!("replaced with a list just above"),
    }
}

fn entry_object(entry: &mut Value) -> &mut Map<String, Value> {
    if let Some(source) = entry.as_str().map(str::to_owned) {
        *entry = json!({ "source": source });
    }
    object_mut(entry)
}

fn set_path(root: &mut Value, path: &[String], value: Value) {
    let mut node = root;
    for (n, key) in path.iter().enumerate() {
        let map = object_mut(node);
        if n + 1 == path.len() {
            map.insert(key.clone(), value);
            return;
        }
        node = map.entry(key.clone()).or_insert_with(|| json!({}));
    }
}

fn unset_path(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else { return };
    if parents.is_empty() {
        if let Some(map) = root.as_object_mut() {
            map.remove(last);
        }
        return;
    }
    if let Some(node) = root.get_mut(&parents[0]) {
        unset_path(node, &path[1..]);
        if node.as_object().is_some_and(|m| m.is_empty())
            && let Some(map) = root.as_object_mut()
        {
            map.remove(&parents[0]);
        }
    }
}

impl App {
    pub fn new(doc: Value, path: PathBuf) -> App {
        App {
            saved_server: server_part(&doc),
            doc,
            path,
            dirty: false,
            infos: HashMap::new(),
            problems: HashMap::new(),
            sel: 0,
            field_sel: 0,
            focus: Focus::List,
            mode: Mode::Normal,
            notice: String::new(),
            notice_is_error: false,
        }
    }

    pub fn say(&mut self, message: impl Into<String>) {
        self.notice = message.into();
        self.notice_is_error = false;
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.notice = message.into();
        self.notice_is_error = true;
    }

    pub fn entries(&self) -> &[Value] {
        self.doc.get("extensions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
    }

    fn entries_mut(&mut self) -> &mut Vec<Value> {
        array_mut(object_mut(&mut self.doc).entry("extensions").or_insert_with(|| json!([])))
    }

    pub fn items(&self) -> Vec<Item> {
        let mut items = vec![Item::Server, Item::Tunnel];
        items.extend((0..self.entries().len()).map(Item::Ext));
        items.push(Item::Add);
        items
    }

    pub fn item(&self) -> Item {
        let items = self.items();
        items[self.sel.min(items.len() - 1)]
    }

    pub fn name_of(&self, index: usize) -> String {
        let entry = &self.entries()[index];
        if let Some(name) = entry.get("name").and_then(Value::as_str) {
            return name.to_owned();
        }
        match self.infos.get(source_of(entry)) {
            Some(info) => info.name.clone(),
            None => crate::commands::entry_name(entry),
        }
    }

    pub fn fields(&self) -> Vec<Field> {
        match self.item() {
            Item::Server => server_fields(),
            Item::Tunnel => tunnel_fields(),
            Item::Ext(i) => {
                let entry = &self.entries()[i];
                match self.infos.get(source_of(entry)) {
                    Some(info) if !info.fields.is_empty() => info.fields.clone(),
                    _ => fields_from_value(entry.get("config").unwrap_or(&Value::Null)),
                }
            }
            Item::Add => Vec::new(),
        }
    }

    pub fn effective(&self, field: &Field) -> Option<Value> {
        self.value(field).cloned().or_else(|| field.default.clone())
    }

    pub fn value(&self, field: &Field) -> Option<&Value> {
        match self.item() {
            Item::Server | Item::Tunnel => get_path(&self.doc, &field.path),
            Item::Ext(i) => self.entries()[i].get("config").and_then(|c| get_path(c, &field.path)),
            Item::Add => None,
        }
    }

    fn set_value(&mut self, field: &Field, value: Option<Value>) {
        match self.item() {
            Item::Server | Item::Tunnel => match value {
                Some(v) => set_path(&mut self.doc, &field.path, v),
                None => unset_path(&mut self.doc, &field.path),
            },
            Item::Ext(i) => {
                let map = entry_object(&mut self.entries_mut()[i]);
                let config = map.entry("config").or_insert_with(|| json!({}));
                match value {
                    Some(v) => set_path(config, &field.path, v),
                    None => unset_path(config, &field.path),
                }
                if config.as_object().is_some_and(|c| c.is_empty()) {
                    map.remove("config");
                }
            }
            Item::Add => return,
        }
        self.dirty = true;
    }

    pub fn toggle(&mut self) {
        match self.item() {
            Item::Tunnel => {
                let on = get_path(&self.doc, &["tunnel".into(), "enabled".into()]).and_then(Value::as_bool).unwrap_or(false);
                set_path(&mut self.doc, &["tunnel".into(), "enabled".into()], Value::Bool(!on));
                self.dirty = true;
            }
            Item::Ext(i) => {
                let now = enabled_of(&self.entries()[i]);
                let map = entry_object(&mut self.entries_mut()[i]);
                if now {
                    map.insert("enabled".into(), Value::Bool(false));
                } else {
                    map.remove("enabled");
                }
                self.dirty = true;
            }
            _ => {}
        }
    }

    pub fn tunnel_on(&self) -> bool {
        get_path(&self.doc, &["tunnel".into(), "enabled".into()]).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        let server_changed = self.saved_server != server_part(&self.doc);
        crate::commands::save_config(&self.path, &self.doc).with_context(|| format!("writing {}", self.path.display()))?;
        self.dirty = false;
        self.saved_server = server_part(&self.doc);
        let missing = self.missing_required();
        let hint = crate::commands::apply_hint(server_changed);
        if missing.is_empty() {
            self.say(format!("Saved. {hint}"));
        } else {
            self.fail(format!("Saved, but required settings are missing: {}. {hint}", missing.join(", ")));
        }
        Ok(())
    }

    pub fn missing_required(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for (i, entry) in self.entries().iter().enumerate() {
            let Some(info) = self.infos.get(source_of(entry)).filter(|_| enabled_of(entry)) else { continue };
            for field in info.fields.iter().filter(|f| f.required && f.default.is_none()) {
                if entry.get("config").and_then(|c| get_path(c, &field.path)).is_none() {
                    missing.push(format!("{}.{}", self.name_of(i), field.label()));
                }
            }
        }
        missing
    }

    fn quit_or_ask(&mut self) -> Effect {
        if self.dirty {
            self.mode = Mode::ConfirmQuit;
            Effect::None
        } else {
            Effect::Quit
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.notice.clear();
        if ctrl && key.code == KeyCode::Char('c') {
            return if self.mode == Mode::ConfirmQuit { Effect::Quit } else { self.quit_or_ask() };
        }
        match self.mode.clone() {
            Mode::Normal => self.key_normal(key.code),
            Mode::Edit(text) => self.key_text(key, text, false),
            Mode::Add(text) => self.key_text(key, text, true),
            Mode::ConfirmDelete => {
                if matches!(key.code, KeyCode::Char('y' | 'Y'))
                    && let Item::Ext(i) = self.item()
                {
                    let name = self.name_of(i);
                    self.entries_mut().remove(i);
                    self.dirty = true;
                    self.say(format!("Removed {name}."));
                }
                self.mode = Mode::Normal;
                Effect::None
            }
            Mode::ConfirmQuit => match key.code {
                KeyCode::Char('y' | 'Y') => match self.save() {
                    Ok(()) => Effect::Quit,
                    Err(e) => {
                        self.fail(format!("{e:#}"));
                        self.mode = Mode::Normal;
                        Effect::None
                    }
                },
                KeyCode::Char('n' | 'N') => Effect::Quit,
                _ => {
                    self.mode = Mode::Normal;
                    Effect::None
                }
            },
        }
    }

    fn key_normal(&mut self, code: KeyCode) -> Effect {
        match code {
            KeyCode::Char('s') => {
                if let Err(e) = self.save() {
                    self.fail(format!("{e:#}"));
                }
            }
            KeyCode::Char('q') => return self.quit_or_ask(),
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') if self.focus == Focus::Fields => self.focus = Focus::List,
            KeyCode::Esc => return self.quit_or_ask(),
            _ => {}
        }
        match self.focus {
            Focus::List => self.key_list(code),
            Focus::Fields => self.key_fields(code),
        }
        Effect::None
    }

    fn key_list(&mut self, code: KeyCode) {
        let last = self.items().len() - 1;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(last),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('a') => self.mode = Mode::Add(String::new()),
            KeyCode::Char('d') | KeyCode::Delete => {
                if matches!(self.item(), Item::Ext(_)) {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self.item() == Item::Add {
                    self.mode = Mode::Add(String::new());
                } else if !self.fields().is_empty() {
                    self.focus = Focus::Fields;
                    self.field_sel = 0;
                } else if matches!(self.item(), Item::Ext(i) if self.problems.contains_key(source_of(&self.entries()[i]))) {
                    self.fail("It couldn't be loaded, so its settings are unknown (see the problem on the right).");
                } else {
                    self.say("This extension has no settings.");
                }
            }
            _ => {}
        }
        self.sel = self.sel.min(self.items().len() - 1);
        self.field_sel = 0;
    }

    fn key_fields(&mut self, code: KeyCode) {
        let fields = self.fields();
        let last = fields.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.field_sel = self.field_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.field_sel = (self.field_sel + 1).min(last),
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
                if let Some(field) = fields.get(self.field_sel) {
                    self.set_value(field, None);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(field) = fields.get(self.field_sel) else { return };
                let current = self.effective(field);
                match &field.kind {
                    Kind::Bool => {
                        let on = current.and_then(|v| v.as_bool()).unwrap_or(false);
                        self.set_value(field, Some(Value::Bool(!on)));
                    }
                    Kind::Choice(options) if !options.is_empty() => {
                        let at = current.as_ref().and_then(Value::as_str).and_then(|c| options.iter().position(|o| o == c));
                        let next = at.map_or(0, |i| (i + 1) % options.len());
                        self.set_value(field, Some(Value::String(options[next].clone())));
                    }
                    _ => {
                        let text = match self.value(field).cloned() {
                            Some(Value::String(s)) => s,
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        self.mode = Mode::Edit(text);
                    }
                }
            }
            _ => {}
        }
    }

    fn key_text(&mut self, key: KeyEvent, mut text: String, adding: bool) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('u') if ctrl => self.mode = if adding { Mode::Add(String::new()) } else { Mode::Edit(String::new()) },
            KeyCode::Char(_) if ctrl => {}
            KeyCode::Char(c) => {
                text.push(c);
                self.mode = if adding { Mode::Add(text) } else { Mode::Edit(text) };
            }
            KeyCode::Backspace => {
                text.pop();
                self.mode = if adding { Mode::Add(text) } else { Mode::Edit(text) };
            }
            KeyCode::Enter if adding => return self.add(text.trim()),
            KeyCode::Enter => {
                let fields = self.fields();
                let Some(field) = fields.get(self.field_sel) else { return Effect::None };
                if text.trim().is_empty() {
                    self.set_value(field, None);
                    self.mode = Mode::Normal;
                } else {
                    match parse_input(&field.kind, &text) {
                        Ok(value) => {
                            self.set_value(field, Some(value));
                            self.mode = Mode::Normal;
                        }
                        Err(why) => self.fail(why),
                    }
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn add(&mut self, source: &str) -> Effect {
        if source.is_empty() {
            self.mode = Mode::Normal;
            return Effect::None;
        }
        if let Err(e) = crate::sources::parse(source, self.path.parent().unwrap_or(std::path::Path::new("."))) {
            self.fail(format!("{e:#}").lines().next().unwrap_or("not a valid source").to_owned());
            return Effect::None;
        }
        let source = crate::commands::local_source_as_absolute(source);
        if self.entries().iter().any(|e| source_of(e) == source) {
            self.fail("It's already in the config.");
            return Effect::None;
        }
        self.entries_mut().push(json!(source));
        self.dirty = true;
        self.sel = self.items().len() - 2;
        self.mode = Mode::Normal;
        Effect::Reload
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyEventKind;

    use super::*;

    fn press(app: &mut App, code: KeyCode) -> Effect {
        app.key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: ratatui::crossterm::event::KeyEventState::NONE })
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn app() -> App {
        let mut app = App::new(
            json!({ "port": 3000, "extensions": ["https://x.example/mio.wasm", { "source": "./b.wasm", "config": { "n": 1 } }] }),
            PathBuf::from("/nonexistent/config.json"),
        );
        app.infos.insert(
            "https://x.example/mio.wasm".into(),
            ExtInfo {
                name: "mio".into(),
                fields: vec![Field::new(&["anilist", "client_id"], Kind::Text, ""), Field::new(&["retries"], Kind::Integer, ""), Field::new(&["debug"], Kind::Bool, "")],
                ..ExtInfo::default()
            },
        );
        app
    }

    #[test]
    fn space_switches_an_extension_off_and_on() {
        let mut app = app();
        app.sel = 2;
        assert_eq!(app.item(), Item::Ext(0));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.doc["extensions"][0], json!({ "source": "https://x.example/mio.wasm", "enabled": false }));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.doc["extensions"][0], json!({ "source": "https://x.example/mio.wasm" }));
        assert!(app.dirty);
    }

    #[test]
    fn edits_are_typed_by_the_schema_and_tidy() {
        let mut app = app();
        app.sel = 2;
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "123");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.doc["extensions"][0]["config"]["anilist"]["client_id"], json!("123"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Edit("x".into()));
        assert!(app.notice.contains("whole number"));
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "4");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.doc["extensions"][0]["config"]["retries"], json!(4));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.doc["extensions"][0]["config"]["debug"], json!(true));
        for _ in 0..2 {
            press(&mut app, KeyCode::Up);
        }
        press(&mut app, KeyCode::Char('x'));
        assert!(app.doc["extensions"][0]["config"].get("anilist").is_none());
    }

    #[test]
    fn server_and_tunnel_settings_go_into_the_document() {
        let mut app = app();
        app.sel = 1;
        press(&mut app, KeyCode::Char(' '));
        assert!(app.tunnel_on());
        assert_eq!(app.doc["tunnel"], json!({ "enabled": true }));
        app.sel = 0;
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        for _ in 0..4 {
            press(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "8080");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.doc["port"], json!(8080));
    }

    #[test]
    fn add_and_remove() {
        let mut app = app();
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "github:someone/weather");
        assert_eq!(press(&mut app, KeyCode::Enter), Effect::Reload);
        assert_eq!(app.entries().len(), 3);
        assert_eq!(app.item(), Item::Ext(2));
        assert_eq!(app.name_of(2), "weather");
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.mode, Mode::ConfirmDelete);
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.entries().len(), 2);
    }

    #[test]
    fn quitting_with_unsaved_changes_asks() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::Char('q')), Effect::Quit);
        app.sel = 1;
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(press(&mut app, KeyCode::Char('q')), Effect::None);
        assert_eq!(app.mode, Mode::ConfirmQuit);
        assert_eq!(press(&mut app, KeyCode::Char('n')), Effect::Quit);
    }

    #[test]
    fn ctrl_c_asks_like_q_then_quits() {
        let mut app = app();
        let ctrl_c = KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, kind: KeyEventKind::Press, state: ratatui::crossterm::event::KeyEventState::NONE };
        app.sel = 1;
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.key(ctrl_c), Effect::None);
        assert_eq!(app.mode, Mode::ConfirmQuit);
        assert_eq!(app.key(ctrl_c), Effect::Quit);
    }

    #[test]
    fn a_setting_on_by_default_turns_off_with_one_press() {
        let mut app = app();
        app.sel = 0;
        press(&mut app, KeyCode::Enter);
        while app.fields()[app.field_sel].label() != "cors" {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.doc["cors"], json!(false));
    }

    #[test]
    fn the_same_local_file_is_not_added_twice() {
        let mut app = app();
        for _ in 0..2 {
            press(&mut app, KeyCode::Char('a'));
            type_text(&mut app, "./same.wasm");
            press(&mut app, KeyCode::Enter);
        }
        assert_eq!(app.entries().len(), 3);
        assert!(app.notice_is_error && app.notice.contains("already"));
    }

    #[test]
    fn a_config_that_would_not_load_is_not_saved() {
        let dir = std::env::temp_dir().join(format!("samishisa-tui-{}", std::process::id()));
        let mut app = App::new(json!({ "port": 3000 }), dir.join("config.json"));
        app.doc["port"] = json!(70000);
        assert!(app.save().is_err());
        assert!(!dir.join("config.json").exists());
        app.doc["port"] = json!(8080);
        app.save().unwrap();
        assert!(app.notice.starts_with("Saved. Restart samishisa to apply."), "{}", app.notice);
        let _ = std::fs::remove_dir_all(dir);
    }
}
