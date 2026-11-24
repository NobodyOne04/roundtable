//! App state and its single update function (ADR 0002).
//! Only the main loop owns `App`; every other task talks to it through `Event`s.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Everything that can happen to the app. Every source sends these into one channel.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Resize,
    /// Redraw throttle: at most one draw per tick.
    Tick,
    /// The terminal input stream ended or failed; nothing more can be typed.
    InputClosed(String),
}

/// What `update` asks the loop to do. The loop executes these; results come back as `Event`s.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Redraw,
    Quit,
}

#[derive(Debug)]
pub struct App {
    /// State changed since the last draw.
    dirty: bool,
}

impl Default for App {
    fn default() -> Self {
        // Dirty from the start, so the first tick draws the screen.
        Self { dirty: true }
    }
}

impl App {
    /// The only place state changes. No I/O here, so it is testable without a terminal.
    pub fn update(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Key(key) if is_quit(key) => vec![Effect::Quit],
            Event::Key(_) => vec![],
            Event::Resize => {
                self.dirty = true;
                vec![]
            }
            Event::Tick if self.dirty => {
                self.dirty = false;
                vec![Effect::Redraw]
            }
            Event::Tick => vec![],
            Event::InputClosed(reason) => {
                tracing::error!(%reason, "terminal input closed");
                vec![Effect::Quit]
            }
        }
    }
}

fn is_quit(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && (key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn quits_on_q_and_ctrl_c_only() {
        let mut app = App::default();
        assert_eq!(
            app.update(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            [Effect::Quit]
        );
        assert_eq!(
            app.update(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            [Effect::Quit]
        );
        assert!(
            app.update(key(KeyCode::Char('c'), KeyModifiers::NONE))
                .is_empty()
        );
        assert!(app.update(key(KeyCode::Esc, KeyModifiers::NONE)).is_empty());
    }

    #[test]
    fn draws_once_per_change_at_most_once_per_tick() {
        let mut app = App::default();
        assert_eq!(
            app.update(Event::Tick),
            [Effect::Redraw],
            "first tick draws"
        );
        assert!(app.update(Event::Tick).is_empty(), "nothing changed");
        app.update(Event::Resize);
        app.update(Event::Resize);
        assert_eq!(
            app.update(Event::Tick),
            [Effect::Redraw],
            "two resizes, one draw"
        );
        assert!(app.update(Event::Tick).is_empty());
    }

    #[test]
    fn quits_when_input_closes() {
        let mut app = App::default();
        assert_eq!(app.update(Event::InputClosed("eof".into())), [Effect::Quit]);
    }
}
