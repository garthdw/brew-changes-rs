use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;

use super::app::{App, Effect, Item, Message, Outcome, changelog_body};
use super::keys::{Keymap, key_name};
use super::theme::Theme;
use super::view;
use crate::github::Client;
use crate::homebrew;

/// Must run before the UI starts reading stdin, or the two race. Terminals
/// that don't answer the query are assumed dark.
pub fn detect_dark_background() -> bool {
    terminal_colorsaurus::theme_mode(Default::default())
        .map_or(true, |mode| mode == terminal_colorsaurus::ThemeMode::Dark)
}

pub async fn run(
    items: Vec<Item>,
    keymap: Keymap,
    themes: Vec<(String, Theme)>,
    active: usize,
    client: Client,
) -> Result<(Outcome, Vec<Item>)> {
    let mut terminal = ratatui::try_init().context("initialising the terminal")?;
    let result = event_loop(
        &mut terminal,
        App::new(items, keymap, themes, active),
        client,
    )
    .await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut app: App,
    client: Client,
) -> Result<(Outcome, Vec<Item>)> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    // Ticks are only polled while busy; don't replay those missed while idle.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let size = terminal.size()?;
    app.update(Message::Resize {
        width: size.width,
        height: size.height,
    });

    loop {
        terminal.draw(|frame| view::draw(frame, &app))?;
        let message = tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => Message::Key(key_name(&key)),
                Some(Ok(Event::Resize(width, height))) => Message::Resize { width, height },
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(error).context("reading terminal events"),
                None => return Ok((Outcome::Quit, app.items)),
            },
            Some(message) = receiver.recv() => message,
            _ = tick.tick(), if app.busy() => Message::Tick,
        };
        for effect in app.update(message) {
            spawn_effect(effect, &app, &client, &sender);
        }
        if let Some(outcome) = app.outcome {
            return Ok((outcome, app.items));
        }
    }
}

/// The receiver only goes away once the UI has exited, at which point a late
/// result has no one left to inform.
fn send(sender: &UnboundedSender<Message>, message: Message) {
    if sender.send(message).is_err() {
        // UI already exited.
    }
}

fn spawn_effect(effect: Effect, app: &App, client: &Client, sender: &UnboundedSender<Message>) {
    match effect {
        Effect::Fetch(index) => {
            let Some(item) = app.items.get(index) else {
                return;
            };
            tokio::spawn({
                let item = item.clone();
                let client = client.clone();
                let sender = sender.clone();
                async move {
                    let changelog = client
                        .fetch(&item.source, &item.installed, &item.current, |stage| {
                            send(&sender, Message::Stage { index, stage });
                        })
                        .await;
                    let body = changelog_body(&item, &changelog);
                    send(
                        &sender,
                        Message::Fetched {
                            index,
                            body,
                            filename: changelog.into_filename(),
                        },
                    );
                }
            });
        }
        Effect::Upgrade(index) => {
            let Some(item) = app.items.get(index) else {
                return;
            };
            tokio::spawn({
                let name = item.name.clone();
                let is_cask = item.is_cask;
                let sender = sender.clone();
                async move {
                    let error = homebrew::upgrade_quiet(&name, is_cask)
                        .await
                        .err()
                        .map(|error| format!("{error:#}"));
                    send(&sender, Message::UpgradeDone { index, error });
                }
            });
        }
        Effect::Open(url) => {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            tokio::spawn({
                let sender = sender.clone();
                async move {
                    let status = tokio::process::Command::new(opener)
                        .arg(&url)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                    let failure = match status {
                        Ok(status) if status.success() => return,
                        Ok(status) => format!("{opener} {url}: {status}"),
                        Err(error) => format!("{opener} {url}: {error}"),
                    };
                    send(&sender, Message::Notice(failure));
                }
            });
        }
    }
}
