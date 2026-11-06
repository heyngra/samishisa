mod model;
pub(crate) mod schema;
mod view;

use std::path::Path;

use anyhow::Context;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::log;
use model::{App, Effect, ExtInfo, source_of};

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let doc = crate::commands::load_config_json(config_path)?;
    let mut app = App::new(doc, config_path.to_path_buf());
    log::set_min_level(log::Level::Off);
    if !app.entries().is_empty() {
        println!("Loading extensions...");
    }
    load_infos(&mut app);

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result?;
    if app.dirty {
        println!("Changes were not saved.");
    }
    Ok(())
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    loop {
        terminal.draw(|frame| view::draw(frame, app))?;
        let Event::Key(key) = event::read().context("reading the keyboard")? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.key(key) {
            Effect::Quit => return Ok(()),
            Effect::Reload => {
                app.say("Fetching the extension...");
                terminal.draw(|frame| view::draw(frame, app))?;
                app.notice.clear();
                load_infos(app);
                if app.notice.is_empty() {
                    report_added(app);
                }
            }
            Effect::None => {}
        }
    }
}

fn report_added(app: &mut App) {
    let Some(last) = app.entries().len().checked_sub(1) else { return };
    let source = source_of(&app.entries()[last]).to_owned();
    let name = app.name_of(last);
    match app.problems.get(&source).cloned() {
        Some(problem) => app.fail(format!("Added {name}, but it couldn't be loaded: {problem}")),
        None => app.say(format!("Added {name}. Press s to save.")),
    }
}

fn load_infos(app: &mut App) {
    app.problems.clear();
    let loaded = match crate::commands::manifests(&app.doc, &app.path, None) {
        Ok(loaded) => loaded,
        Err(e) => {
            app.fail(format!("The config has a problem: {e:#}"));
            return;
        }
    };
    for (source, manifest) in loaded {
        match manifest {
            Ok(manifest) => {
                let fields = manifest.config.as_ref().map(schema::fields_of).unwrap_or_default();
                app.infos.insert(source, ExtInfo { name: manifest.name, version: manifest.version, description: manifest.description, routes: manifest.routes.len(), fields });
            }
            Err(e) => {
                app.infos.remove(&source);
                app.problems.insert(source, format!("{e:#}").lines().next().unwrap_or("failed").to_owned());
            }
        }
    }
}
