use std::{
    borrow::{Borrow, BorrowMut},
    sync::{Arc, LazyLock},
};

use eyre::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::ResourceExt;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    prelude::*,
    style::{palette::tailwind, Modifier, Style},
    text::Line,
    widgets::{
        self, block::Title, Block, Borders, Clear, Paragraph, Row, StatefulWidget,
        StatefulWidgetRef, Table, TableState, Widget as _, WidgetRef,
    },
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::{as_24_bit_terminal_escaped, LinesWithEndings},
};
use syntect_tui::into_span;
use tokio_util::time::delay_queue::Key;
use tracing::info;

use super::{
    log::Log,
    tabs::{Tab, TabbedView},
    yaml, Widget,
};
use crate::{
    events::{Broadcast, Event, Keypress},
    resources::{
        pod::{self, PodExt},
        store::Store,
        Yaml as YamlResource,
    },
    widget::{propagate, yaml::Yaml, Dispatch, Screen, TableRow},
};

struct RowStyle {
    healthy: Style,
    unhealthy: Style,
    normal: Style,
}

impl Default for RowStyle {
    fn default() -> Self {
        Self {
            healthy: Style::default().fg(tailwind::GREEN.c300),
            unhealthy: Style::default().fg(tailwind::RED.c300),
            normal: Style::default().fg(tailwind::INDIGO.c300),
        }
    }
}

struct TableStyle {
    border: Style,
    header: Style,
    selected: Style,
    row: RowStyle,
}

impl Default for TableStyle {
    fn default() -> Self {
        Self {
            border: Style::default(),
            header: Style::default().bold(),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            row: RowStyle::default(),
        }
    }
}

// - Handle items being removed/added
// - Render scrollbar only if there's something that needs to be scrolled.
pub struct PodTable {
    client: kube::Client,
    pods: Store<Pod>,
    table: TableState,
    cmd: Option<Command>,
    detail: Option<Detail>,
}

impl PodTable {
    pub fn new(client: kube::Client) -> Self {
        Self {
            client: client.clone(),
            pods: Store::new(client),
            table: TableState::default().with_selected(0),

            cmd: None,
            detail: None,
        }
    }

    fn items(&self) -> Vec<Arc<Pod>> {
        let filter = self.cmd.as_ref().map(Command::content);

        if filter.is_none() {
            return self.pods.state();
        }

        self.pods
            .state()
            .into_iter()
            .filter(|pod| {
                let filter = filter.unwrap();

                if filter.is_empty() {
                    return true;
                }

                pod.matches(filter)
            })
            .collect()
    }

    fn scroll(&mut self, key: &Keypress) {
        let current = self.table.selected().unwrap_or_default();

        let next = match key {
            Keypress::CursorUp => current.saturating_sub(1),
            Keypress::CursorDown => current.saturating_add(1),
            _ => return,
        };

        let max = self.items().len().saturating_sub(1);

        self.table.select(Some(next.clamp(0, max)));
    }

    fn list(&mut self, frame: &mut Frame, area: Rect) {
        let style = TableStyle::default();

        let border = Block::default()
            .title("Pods")
            .borders(Borders::ALL)
            .style(style.border);

        let state = self.items();

        if self.table.selected().unwrap_or_default() > state.len() {
            self.table.select(Some(state.len().saturating_sub(1)));
        }

        let rows: Vec<Row> = state
            .iter()
            .map(|pod| {
                let row = pod.row();

                match pod.status() {
                    pod::Phase::Pending | pod::Phase::Running => row.style(style.row.normal),
                    pod::Phase::Succeeded => row.style(style.row.healthy),
                    pod::Phase::Unknown(_) => row.style(style.row.unhealthy),
                }
            })
            .collect();

        let table = Table::new(rows, Pod::constraints())
            .header(Pod::header().style(style.header))
            .block(border)
            .highlight_style(style.selected);
        frame.render_stateful_widget(&table, area, &mut self.table);
    }

    fn detail(&mut self, frame: &mut Frame, area: Rect) {
        self.detail.as_mut().unwrap().draw(frame, area);
    }
}

impl Dispatch for PodTable {
    fn dispatch(&mut self, event: &Event) -> Result<Broadcast> {
        let Event::Keypress(key) = event else {
            return Ok(Broadcast::Ignored);
        };

        propagate!(self.cmd, event);
        propagate!(self.detail, event);

        match key {
            Keypress::Escape => return Ok(Broadcast::Exited),
            Keypress::Enter => {
                self.detail = self
                    .items()
                    .get(self.table.selected().unwrap_or_default())
                    .map(|pod| Detail::new(self.client.clone(), pod.clone()));
            }
            Keypress::CursorUp | Keypress::CursorDown => self.scroll(key),
            Keypress::Printable(x) => {
                if x == "/" {
                    self.cmd = Some(Command::new());
                }
            }
            _ => {
                return Ok(Broadcast::Ignored);
            }
        };

        Ok(Broadcast::Consumed)
    }
}

impl Screen for PodTable {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let [_, cmd_area] =
            Layout::vertical([Constraint::Fill(0), Constraint::Length(3)]).areas(area);

        if self.detail.is_some() {
            self.detail(frame, area);
        } else {
            self.list(frame, area);
        }

        if self.cmd.is_none() {
            return;
        }

        // Command ends up being written *over the table (which writes to the whole
        // screen). The clear makes sure that table items don't show up weirdly behind a
        // transparent command buffer.
        frame.render_widget(Clear, cmd_area);

        self.cmd.as_mut().unwrap().draw(frame, cmd_area);
    }
}

struct Command {
    content: String,
    pos: u16,
}

impl Command {
    fn new() -> Self {
        Self {
            content: String::new(),
            pos: 0,
        }
    }

    fn content(&self) -> &str {
        self.content.as_str()
    }
}

impl Dispatch for Command {
    fn dispatch(&mut self, event: &Event) -> Result<Broadcast> {
        match event {
            Event::Keypress(Keypress::Escape) => {
                return Ok(Broadcast::Exited);
            }
            Event::Keypress(Keypress::Printable(x)) => {
                self.content.insert_str(self.pos as usize, x);
                self.pos = self.pos.saturating_add(1);
            }
            Event::Keypress(Keypress::Backspace) => 'outer: {
                if self.content.is_empty() || self.pos == 0 {
                    break 'outer;
                }

                self.content.remove(self.pos as usize - 1);
                self.pos = self.pos.saturating_sub(1);
            }
            Event::Keypress(Keypress::CursorLeft) => {
                self.pos = self.pos.saturating_sub(1);
            }
            #[allow(clippy::cast_possible_truncation)]
            Event::Keypress(Keypress::CursorRight) => {
                self.pos = self
                    .pos
                    .saturating_add(1)
                    .clamp(0, self.content.len() as u16);
            }
            _ => {
                return Ok(Broadcast::Ignored);
            }
        };

        Ok(Broadcast::Consumed)
    }
}

impl Screen for Command {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default().title("Command").borders(Borders::ALL);

        let cmd_pos = block.inner(area);

        let pg = Paragraph::new(self.content()).block(block);

        frame.render_widget(pg, area);

        frame.set_cursor(cmd_pos.x + self.pos, cmd_pos.y);
    }
}

struct DetailStyle {
    breadcrumb: Style,
}

impl Default for DetailStyle {
    fn default() -> Self {
        Self {
            breadcrumb: Style::default().add_modifier(Modifier::BOLD),
        }
    }
}

struct Detail {
    client: kube::Client,
    pod: Arc<Pod>,

    view: TabbedView,
}

impl Detail {
    fn new(client: kube::Client, pod: Arc<Pod>) -> Self {
        let _pod = pod.clone();
        let yaml = Tab::new(
            "Overview".to_string(),
            Box::new(move || Box::new(Yaml::new(_pod.clone()))),
        );

        let _pod = pod.clone();
        let _client = client.clone();
        let logs = Tab::new(
            "Logs".to_string(),
            Box::new(move || Box::new(Log::new(_client.clone(), _pod.clone()))),
        );

        let view = TabbedView::new(vec![yaml, logs]).unwrap();

        Self { client, pod, view }
    }

    fn breadcrumb(&self) -> Vec<Span> {
        let style = DetailStyle::default();

        let mut crumb: Vec<Span> = Vec::new();

        if let Some(ns) = self.pod.namespace() {
            crumb.push(ns.into());
            crumb.push(Span::from(" → ").style(style.breadcrumb));
        }

        crumb.push(self.pod.name_any().into());

        crumb
    }
}

impl Dispatch for Detail {
    fn dispatch(&mut self, event: &Event) -> Result<Broadcast> {
        if matches!(self.view.dispatch(event)?, Broadcast::Consumed) {
            return Ok(Broadcast::Consumed);
        }

        let Event::Keypress((key)) = event else {
            return Ok(Broadcast::Ignored);
        };

        if matches!(key, Keypress::Escape) {
            return Ok(Broadcast::Exited);
        }

        Ok(Broadcast::Ignored)
    }
}

impl Screen for Detail {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(self.breadcrumb()));

        let inner = block.inner(area);

        frame.render_widget(block, area);

        self.view.draw(frame, inner);
    }
}
