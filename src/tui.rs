use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    config::AppConfig,
    index::{IndexManager, SearchIndex},
    resume::{CommandSpec, build_resume_command, run_command},
    scan::{ScanMode, load_scan_docs, search_docs},
    types::{MessageDoc, Role, SearchOptions, SessionHit},
};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(33);
const PREVIEW_CONVERSATION_LIMIT: usize = 200;
const PREVIEW_RECENT_MESSAGES: usize = 2;

pub fn run_tui(
    manager: IndexManager,
    config: AppConfig,
    initial_query: String,
    no_alt_screen: bool,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let use_alt_screen = !no_alt_screen;
    if use_alt_screen {
        execute!(stdout, EnterAlternateScreen)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(initial_query, config);
    app.status = "loading index...".to_string();
    terminal.draw(|frame| app.render(frame))?;

    let result = (|| -> Result<Option<CommandSpec>> {
        let mut index = manager.load_search_index()?;
        if index.is_empty() {
            app.backend = SearchBackend::Fuzzy;
            app.status =
                "index empty: using fuzzy scan. press u to update index, R to rebuild".to_string();
            terminal.draw(|frame| app.render(frame))?;
        }
        app.refresh(&index);
        run_loop(&mut terminal, &manager, &mut app, &mut index)
    })();

    disable_raw_mode()?;
    if use_alt_screen {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    }
    terminal.show_cursor()?;

    if let Some(command) = result? {
        std::process::exit(run_command(&command)?);
    }
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    manager: &IndexManager,
    app: &mut TuiApp,
    index: &mut SearchIndex,
) -> Result<Option<CommandSpec>> {
    loop {
        terminal.draw(|frame| app.render(frame))?;
        if event::poll(app.poll_timeout())? {
            match event::read()? {
                Event::Key(key) => match app.handle_key(key, index)? {
                    TuiControl::Continue => {}
                    TuiControl::Quit => break Ok(None),
                    TuiControl::Resume(command) => break Ok(Some(command)),
                    TuiControl::UpdateIndex => {
                        app.status = "updating index...".to_string();
                        terminal.draw(|frame| app.render(frame))?;
                        match manager.update() {
                            Ok(stats) => {
                                *index = manager.load_search_index()?;
                                app.backend = SearchBackend::Index;
                                app.refresh(index);
                                app.status = format!(
                                    "index updated: {} docs in {} segments",
                                    stats.docs, stats.segments
                                );
                            }
                            Err(error) => {
                                app.status = format!("index update failed: {error}");
                            }
                        }
                    }
                    TuiControl::RebuildIndex => {
                        app.status = "rebuilding index...".to_string();
                        terminal.draw(|frame| app.render(frame))?;
                        match manager.rebuild() {
                            Ok(stats) => {
                                *index = manager.load_search_index()?;
                                app.backend = SearchBackend::Index;
                                app.refresh(index);
                                app.status = format!(
                                    "index rebuilt: {} docs in {} segments",
                                    stats.docs, stats.segments
                                );
                            }
                            Err(error) => {
                                app.status = format!("index rebuild failed: {error}");
                            }
                        }
                    }
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if app.pending_search_due() {
            app.status = "searching...".to_string();
            terminal.draw(|frame| app.render(frame))?;
            app.run_pending_search(index);
        }
    }
}

enum TuiControl {
    Continue,
    Quit,
    Resume(CommandSpec),
    UpdateIndex,
    RebuildIndex,
}

struct TuiApp {
    query: LineEditor,
    prompt: LineEditor,
    mode: InputMode,
    backend: SearchBackend,
    scan_docs: Option<Vec<MessageDoc>>,
    results: Vec<SessionHit>,
    conversation: Vec<MessageDoc>,
    recent_conversation: Vec<MessageDoc>,
    preview_scroll: usize,
    selected: usize,
    profile: String,
    config: AppConfig,
    status: String,
    pending_search: bool,
    last_query_edit: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Query,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchBackend {
    Index,
    Literal,
    Regex,
    Fuzzy,
}

#[derive(Debug, Clone, Default)]
struct LineEditor {
    text: String,
    cursor: usize,
}

impl LineEditor {
    fn new(text: String) -> Self {
        Self {
            cursor: text.len(),
            text,
        }
    }

    fn as_str(&self) -> &str {
        &self.text
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn cursor_chars(&self) -> u16 {
        self.text[..self.cursor].chars().count() as u16
    }

    fn insert(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_boundary(self.cursor);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.next_boundary(self.cursor);
        self.text.drain(self.cursor..next);
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn delete_word_before_cursor(&mut self) {
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .last()
                .is_some_and(char::is_whitespace)
        {
            self.backspace();
        }
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .last()
                .is_some_and(|ch| !ch.is_whitespace())
        {
            self.backspace();
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    fn move_right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    fn prev_boundary(&self, idx: usize) -> usize {
        self.text[..idx]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0)
    }

    fn next_boundary(&self, idx: usize) -> usize {
        self.text[idx..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| idx + offset)
            .unwrap_or(self.text.len())
    }
}

impl SearchBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Literal => "literal-scan",
            Self::Regex => "regex-scan",
            Self::Fuzzy => "fuzzy-scan",
        }
    }

    fn scan_mode(self) -> Option<ScanMode> {
        match self {
            Self::Index => None,
            Self::Literal => Some(ScanMode::Literal),
            Self::Regex => Some(ScanMode::Regex),
            Self::Fuzzy => Some(ScanMode::Fuzzy),
        }
    }
}

impl TuiApp {
    fn new(query: String, config: AppConfig) -> Self {
        let mode = if query.is_empty() {
            InputMode::Normal
        } else {
            InputMode::Query
        };
        Self {
            query: LineEditor::new(query),
            prompt: LineEditor::default(),
            mode,
            backend: SearchBackend::Index,
            scan_docs: None,
            results: Vec::new(),
            conversation: Vec::new(),
            recent_conversation: Vec::new(),
            preview_scroll: 0,
            selected: 0,
            profile: "default".to_string(),
            config,
            status: "normal: j/k move, / search, e prompt, Enter resume, q quit".to_string(),
            pending_search: false,
            last_query_edit: None,
        }
    }

    fn refresh(&mut self, index: &SearchIndex) {
        self.pending_search = false;
        self.last_query_edit = None;
        let options = SearchOptions {
            limit: 100,
            default_sidechain: Some(false),
            current_cwd: std::env::current_dir().ok(),
        };
        let results = match self.backend.scan_mode() {
            None if self.query.is_empty() => {
                self.results = index.recent_sessions(options.limit, options.default_sidechain);
                self.status =
                    "recent sessions: / search, j/k move, Enter resume, R rebuild".to_string();
                if self.selected >= self.results.len() {
                    self.selected = self.results.len().saturating_sub(1);
                }
                self.update_conversation(index);
                return;
            }
            None => Ok(index.search(self.query.as_str(), options)),
            Some(mode) => {
                if self.query.is_empty() {
                    self.results.clear();
                    self.conversation.clear();
                    self.recent_conversation.clear();
                    self.status =
                        "scan backend idle: type / to search, or press R to build the index"
                            .to_string();
                    return;
                }
                if self.scan_docs.is_none() {
                    self.status = "loading scan corpus...".to_string();
                    self.scan_docs = match load_scan_docs(&self.config) {
                        Ok(docs) => Some(docs),
                        Err(error) => {
                            self.status = format!("scan corpus failed: {error}");
                            Some(Vec::new())
                        }
                    };
                }
                search_docs(
                    self.scan_docs.as_deref().unwrap_or(&[]),
                    self.query.as_str(),
                    mode,
                    options,
                )
            }
        };
        match results {
            Ok(results) => {
                self.results = results;
                if !self.query.is_empty() {
                    self.status = format!(
                        "{} results for `{}`",
                        self.results.len(),
                        compact_status_text(self.query.as_str())
                    );
                }
            }
            Err(error) => {
                self.status = format!("search failed: {error}");
                self.results.clear();
            }
        }
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
        self.update_conversation(index);
    }

    fn schedule_search(&mut self) {
        self.pending_search = true;
        self.last_query_edit = Some(Instant::now());
        self.status = format!(
            "typing: search after {}ms idle",
            SEARCH_DEBOUNCE.as_millis()
        );
    }

    fn pending_search_due(&self) -> bool {
        self.pending_search
            && self
                .last_query_edit
                .is_some_and(|edited_at| edited_at.elapsed() >= SEARCH_DEBOUNCE)
    }

    fn poll_timeout(&self) -> Duration {
        if !self.pending_search {
            return EVENT_POLL_INTERVAL;
        }
        let Some(edited_at) = self.last_query_edit else {
            return EVENT_POLL_INTERVAL;
        };
        SEARCH_DEBOUNCE
            .saturating_sub(edited_at.elapsed())
            .min(EVENT_POLL_INTERVAL)
    }

    fn run_pending_search(&mut self, index: &SearchIndex) {
        if self.pending_search_due() {
            self.refresh(index);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, index: &mut SearchIndex) -> Result<TuiControl> {
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers == KeyModifiers::CONTROL {
            return Ok(TuiControl::Quit);
        }
        if key.code == KeyCode::Esc {
            return match self.mode {
                InputMode::Normal => Ok(TuiControl::Quit),
                InputMode::Query | InputMode::Prompt => {
                    self.mode = InputMode::Normal;
                    self.status =
                        "normal: j/k move, / search, e prompt, m backend, Enter resume, q quit"
                            .to_string();
                    Ok(TuiControl::Continue)
                }
            };
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => match self.mode {
                InputMode::Normal => {
                    if let Some(command) = self.command_for_selected()? {
                        return Ok(TuiControl::Resume(command));
                    }
                }
                InputMode::Query => {
                    self.mode = InputMode::Normal;
                    self.refresh(index);
                }
                InputMode::Prompt => self.mode = InputMode::Normal,
            },
            (KeyCode::Up, _) => self.move_up(index),
            (KeyCode::Down, _) => self.move_down(index),
            (KeyCode::Left, _) => {
                self.active_editor_mut().map(LineEditor::move_left);
            }
            (KeyCode::Right, _) => {
                self.active_editor_mut().map(LineEditor::move_right);
            }
            (KeyCode::Home, _) => {
                self.active_editor_mut().map(LineEditor::move_home);
            }
            (KeyCode::End, _) => {
                self.active_editor_mut().map(LineEditor::move_end);
            }
            (KeyCode::Delete, _) => {
                if let Some(editor) = self.active_editor_mut() {
                    editor.delete();
                    if self.mode == InputMode::Query {
                        self.schedule_search();
                    }
                }
            }
            (KeyCode::Backspace, _) => match self.mode {
                InputMode::Normal => {}
                InputMode::Query => {
                    self.query.backspace();
                    self.schedule_search();
                }
                InputMode::Prompt => {
                    self.prompt.backspace();
                }
            },
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.active_editor_mut().map(LineEditor::move_home);
            }
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.active_editor_mut().map(LineEditor::move_end);
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => match self.mode {
                InputMode::Normal => self.scroll_preview_up(8),
                InputMode::Query => {
                    self.query.clear();
                    self.schedule_search();
                }
                InputMode::Prompt => self.prompt.clear(),
            },
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => match self.mode {
                InputMode::Normal => {}
                InputMode::Query => {
                    self.query.delete_word_before_cursor();
                    self.schedule_search();
                }
                InputMode::Prompt => self.prompt.delete_word_before_cursor(),
            },
            (KeyCode::Char('d'), KeyModifiers::CONTROL) if self.mode == InputMode::Normal => {
                self.scroll_preview_down(8);
            }
            (KeyCode::Char('f'), KeyModifiers::CONTROL) if self.mode == InputMode::Normal => {
                self.scroll_preview_down(16);
            }
            (KeyCode::Char('b'), KeyModifiers::CONTROL) if self.mode == InputMode::Normal => {
                self.scroll_preview_up(16);
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) | (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
                return self.handle_char(ch, index);
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => return Ok(TuiControl::UpdateIndex),
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.cycle_profile(),
            (KeyCode::Char('y'), KeyModifiers::ALT) | (KeyCode::Char('Y'), KeyModifiers::ALT) => {
                self.set_profile("yolo");
            }
            (KeyCode::Char('s'), KeyModifiers::ALT) | (KeyCode::Char('S'), KeyModifiers::ALT) => {
                self.set_profile("safe");
            }
            _ => {}
        }
        Ok(TuiControl::Continue)
    }

    fn handle_char(&mut self, ch: char, index: &mut SearchIndex) -> Result<TuiControl> {
        match self.mode {
            InputMode::Normal => match ch {
                'q' => Ok(TuiControl::Quit),
                'j' => {
                    self.move_down(index);
                    Ok(TuiControl::Continue)
                }
                'k' => {
                    self.move_up(index);
                    Ok(TuiControl::Continue)
                }
                'g' => {
                    self.selected = 0;
                    self.update_conversation(index);
                    Ok(TuiControl::Continue)
                }
                'G' => {
                    self.selected = self.results.len().saturating_sub(1);
                    self.update_conversation(index);
                    Ok(TuiControl::Continue)
                }
                '/' | 'i' => {
                    self.mode = InputMode::Query;
                    self.status = "search mode: type query, Enter/Esc back to normal".to_string();
                    Ok(TuiControl::Continue)
                }
                'e' => {
                    self.mode = InputMode::Prompt;
                    self.status =
                        "prompt mode: type prompt for resume, Enter/Esc back to normal".to_string();
                    Ok(TuiControl::Continue)
                }
                'd' => {
                    self.set_profile("default");
                    Ok(TuiControl::Continue)
                }
                's' => {
                    self.set_profile("safe");
                    Ok(TuiControl::Continue)
                }
                'y' => {
                    self.set_profile("yolo");
                    Ok(TuiControl::Continue)
                }
                'p' => {
                    self.cycle_profile();
                    Ok(TuiControl::Continue)
                }
                'm' => {
                    self.cycle_backend(index);
                    Ok(TuiControl::Continue)
                }
                'u' => Ok(TuiControl::UpdateIndex),
                'R' => Ok(TuiControl::RebuildIndex),
                '?' => {
                    self.status =
                        "keys: j/k g/G / e m d/s/y u R C-d/C-u Enter q; edit: arrows C-a/C-e/C-u/C-w"
                            .to_string();
                    Ok(TuiControl::Continue)
                }
                _ => Ok(TuiControl::Continue),
            },
            InputMode::Query => {
                self.query.insert(ch);
                self.schedule_search();
                Ok(TuiControl::Continue)
            }
            InputMode::Prompt => {
                self.prompt.insert(ch);
                Ok(TuiControl::Continue)
            }
        }
    }

    fn move_up(&mut self, index: &SearchIndex) {
        self.selected = self.selected.saturating_sub(1);
        self.update_conversation(index);
    }

    fn move_down(&mut self, index: &SearchIndex) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1).min(self.results.len() - 1);
            self.update_conversation(index);
        }
    }

    fn set_profile(&mut self, profile: &str) {
        self.profile = profile.to_string();
        self.status = format!("profile set to {profile}");
    }

    fn cycle_backend(&mut self, index: &SearchIndex) {
        self.backend = match self.backend {
            SearchBackend::Index => SearchBackend::Fuzzy,
            SearchBackend::Fuzzy => SearchBackend::Regex,
            SearchBackend::Regex => SearchBackend::Literal,
            SearchBackend::Literal => SearchBackend::Index,
        };
        self.status = format!("backend set to {}", self.backend.as_str());
        self.refresh(index);
    }

    fn active_editor_mut(&mut self) -> Option<&mut LineEditor> {
        match self.mode {
            InputMode::Normal => None,
            InputMode::Query => Some(&mut self.query),
            InputMode::Prompt => Some(&mut self.prompt),
        }
    }

    fn update_conversation(&mut self, index: &SearchIndex) {
        let Some(hit) = self.results.get(self.selected) else {
            self.conversation.clear();
            self.recent_conversation.clear();
            self.preview_scroll = 0;
            return;
        };
        let all_scan_docs = self.backend.scan_mode().and_then(|_| {
            self.scan_docs
                .as_deref()
                .map(|docs| session_docs_from_scan(docs, hit.provider, &hit.session_id))
        });
        self.conversation = match &all_scan_docs {
            None => index.session_docs(hit.provider, &hit.session_id, PREVIEW_CONVERSATION_LIMIT),
            Some(docs) => docs
                .iter()
                .take(PREVIEW_CONVERSATION_LIMIT)
                .cloned()
                .collect(),
        };
        self.recent_conversation = match all_scan_docs {
            None => index.session_docs(hit.provider, &hit.session_id, usize::MAX),
            Some(docs) => docs,
        };
        self.preview_scroll = 0;
    }

    fn scroll_preview_down(&mut self, amount: usize) {
        self.preview_scroll = self.preview_scroll.saturating_add(amount);
    }

    fn scroll_preview_up(&mut self, amount: usize) {
        self.preview_scroll = self.preview_scroll.saturating_sub(amount);
    }

    fn cycle_profile(&mut self) {
        self.profile = match self.profile.as_str() {
            "default" => "safe",
            "safe" => "yolo",
            _ => "default",
        }
        .to_string();
    }

    fn command_for_selected(&self) -> Result<Option<CommandSpec>> {
        let Some(hit) = self.results.get(self.selected) else {
            return Ok(None);
        };
        let command = build_resume_command(
            &self.config,
            hit.provider,
            &self.profile,
            &hit.session_id,
            (!self.prompt.is_empty()).then_some(self.prompt.as_str()),
            hit.cwd.clone(),
        )?;
        Ok(Some(command))
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(6),
            ])
            .split(area);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(chunks[2]);

        self.render_header(frame, chunks[0]);

        let query_title = match self.mode {
            InputMode::Normal => "query",
            InputMode::Query => "query [editing]",
            InputMode::Prompt => "query",
        };
        let query = Paragraph::new(self.query.as_str())
            .block(Block::default().title(query_title).borders(Borders::ALL));
        frame.render_widget(query, chunks[1]);
        self.render_cursor(frame, chunks[1], chunks[3]);

        let items = self
            .results
            .iter()
            .map(|hit| {
                let date = hit
                    .timestamp
                    .map(|ts| ts.format("%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "-- --:--".to_string());
                let project = project_name(hit.cwd.as_deref());
                let title = if self.query.is_empty() {
                    format!(
                        "{} {} {} {}",
                        date,
                        project,
                        hit.provider,
                        short_id(&hit.session_id),
                    )
                } else {
                    format!(
                        "{} {} {:.1} {} {}",
                        date,
                        project,
                        hit.score,
                        hit.provider,
                        short_id(&hit.session_id)
                    )
                };
                ListItem::new(Line::from(title))
            })
            .collect::<Vec<_>>();
        let items = if items.is_empty() {
            vec![
                ListItem::new(Line::from("No results yet")),
                ListItem::new(Line::from(
                    "/ search   R build index   m switch backend   q quit",
                )),
            ]
        } else {
            items
        };
        let mut state = ListState::default();
        if !self.results.is_empty() {
            state.select(Some(self.selected));
        }
        let list = List::new(items)
            .block(Block::default().title("sessions").borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, body[0], &mut state);

        self.render_preview(frame, body[1]);

        let footer_text = vec![
            Line::from(vec![
                Span::raw("profile: "),
                Span::styled(
                    self.profile.as_str(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" | "),
                Span::raw(format!("backend: {}", self.backend.as_str())),
                Span::raw(" | "),
                Span::raw(match self.mode {
                    InputMode::Normal => "mode: normal",
                    InputMode::Query => "mode: query",
                    InputMode::Prompt => "mode: prompt",
                }),
                Span::raw(" | "),
                Span::raw(self.status.as_str()),
            ]),
            Line::from(format!("prompt: {}", self.prompt.as_str())),
            Line::from(
                "keys: j/k move  / search  e prompt  m backend  d/s/y profile  u update  R rebuild  Enter resume  q quit",
            ),
            Line::from(
                self.command_for_selected()
                    .ok()
                    .flatten()
                    .map(|c| c.display())
                    .unwrap_or_default(),
            ),
        ];
        let footer = Paragraph::new(footer_text)
            .block(Block::default().title("command").borders(Borders::ALL))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, chunks[3]);
    }

    fn render_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let title = Line::from(vec![
            Span::styled(
                " lastai ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Codex/Claude history search  "),
            Span::styled(
                format!("{}  ", self.backend.as_str()),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("{} results", self.results.len()),
                Style::default().fg(Color::Yellow),
            ),
        ]);
        let header = Paragraph::new(title).block(Block::default().borders(Borders::ALL));
        frame.render_widget(header, area);
    }

    fn render_preview(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let block = Block::default().title("preview").borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let recent_lines = self.recent_preview_lines();
        let recent_height = if recent_lines.is_empty() {
            0
        } else {
            (recent_lines.len() as u16).min(inner.height)
        };

        if recent_height == 0 {
            let preview = Paragraph::new(self.preview_lines()).wrap(Wrap { trim: false });
            frame.render_widget(preview, inner);
            return;
        }

        let separator_height = u16::from(inner.height > recent_height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(recent_height),
                Constraint::Length(separator_height),
                Constraint::Min(0),
            ])
            .split(inner);
        let recent_line_count = recent_lines.len();
        let mut visible_recent_lines = recent_lines
            .into_iter()
            .take(recent_height as usize)
            .collect::<Vec<_>>();
        if recent_line_count > recent_height as usize
            && let Some(last) = visible_recent_lines.last_mut()
        {
            *last = Line::from(vec![Span::styled(
                "... recent messages truncated by window height",
                Style::default().fg(Color::Yellow),
            )]);
        }
        let recent = Paragraph::new(visible_recent_lines).wrap(Wrap { trim: false });
        frame.render_widget(recent, chunks[0]);

        if separator_height > 0 {
            let separator = Paragraph::new("-".repeat(chunks[1].width as usize));
            frame.render_widget(separator, chunks[1]);
        }

        let preview = Paragraph::new(self.preview_lines()).wrap(Wrap { trim: false });
        frame.render_widget(preview, chunks[2]);
    }

    fn render_cursor(&self, frame: &mut ratatui::Frame<'_>, query_area: Rect, footer_area: Rect) {
        match self.mode {
            InputMode::Normal => {}
            InputMode::Query => {
                let max_x = query_area.x + query_area.width.saturating_sub(2);
                let x = (query_area.x + 1 + self.query.cursor_chars()).min(max_x);
                frame.set_cursor_position(Position::new(x, query_area.y + 1));
            }
            InputMode::Prompt => {
                let prompt_prefix = "prompt: ".len() as u16;
                let max_x = footer_area.x + footer_area.width.saturating_sub(2);
                let x = (footer_area.x + 1 + prompt_prefix + self.prompt.cursor_chars()).min(max_x);
                frame.set_cursor_position(Position::new(x, footer_area.y + 2));
            }
        }
    }

    fn preview_lines(&self) -> Vec<Line<'_>> {
        let Some(hit) = self.results.get(self.selected) else {
            return vec![
                Line::from(vec![Span::styled(
                    "No session selected",
                    Style::default().fg(Color::Yellow),
                )]),
                Line::from(""),
                Line::from("What to do:"),
                Line::from("  / or i   search history"),
                Line::from("  R        build the index from Codex/Claude JSONL"),
                Line::from("  u        update the index after new conversations"),
                Line::from("  m        switch backend: index / fuzzy / regex / literal"),
                Line::from(""),
                Line::from("If the screen looks blank in your terminal:"),
                Line::from("  LASTAI_NO_ALT_SCREEN=1 lastai"),
                Line::from("  or: lastai tui --no-alt-screen"),
            ];
        };
        let mut lines = vec![
            Line::from(format!(
                "{}  {}  {}",
                hit.timestamp
                    .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "unknown-date".to_string()),
                project_name(hit.cwd.as_deref()),
                hit.provider,
            )),
            Line::from(hit.session_id.clone()),
            Line::from(
                hit.cwd
                    .as_ref()
                    .map(|cwd| cwd.display().to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
            Line::from(""),
        ];
        if self.conversation.is_empty() {
            lines.push(Line::from("No conversation cache for this session"));
            for snippet in &hit.snippets {
                lines.push(Line::from(format!(
                    "[{}] {}",
                    snippet.role,
                    snippet
                        .timestamp
                        .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_default()
                )));
                for line in snippet.text.lines().take(8) {
                    lines.push(Line::from(line.to_string()));
                }
                lines.push(Line::from(""));
            }
            return lines.into_iter().skip(self.preview_scroll).collect();
        }
        lines.push(Line::from(format!(
            "conversation: {} messages  scroll: {}",
            self.conversation.len(),
            self.preview_scroll
        )));
        lines.push(Line::from(""));
        for doc in &self.conversation {
            push_doc_preview_lines(&mut lines, doc);
        }
        lines.into_iter().skip(self.preview_scroll).collect()
    }

    fn recent_preview_lines(&self) -> Vec<Line<'_>> {
        let recent = self
            .recent_conversation
            .iter()
            .filter(|doc| doc.role == Role::User)
            .rev()
            .take(PREVIEW_RECENT_MESSAGES)
            .collect::<Vec<_>>();
        if recent.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![
            Line::from(vec![Span::styled(
                "recent messages",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
        ];
        for doc in recent.into_iter().rev() {
            push_recent_doc_preview_lines(&mut lines, doc);
        }
        lines
    }
}

fn session_docs_from_scan(
    docs: &[MessageDoc],
    provider: crate::types::Provider,
    session_id: &str,
) -> Vec<MessageDoc> {
    let mut docs = docs
        .iter()
        .filter(|doc| doc.provider == provider && doc.session_id == session_id)
        .cloned()
        .collect::<Vec<_>>();
    sort_preview_docs(&mut docs);
    docs
}

fn sort_preview_docs(docs: &mut [MessageDoc]) {
    docs.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.source.path.cmp(&b.source.path))
            .then_with(|| a.source.line_number.cmp(&b.source.line_number))
    });
}

fn push_doc_preview_lines<'a>(lines: &mut Vec<Line<'a>>, doc: &'a MessageDoc) {
    lines.push(Line::from(vec![
        Span::styled(
            format!("[{}]", doc.role),
            Style::default().fg(match doc.role.as_str() {
                "user" => Color::Cyan,
                "assistant" => Color::Green,
                "tool" => Color::Yellow,
                "system" => Color::Magenta,
                _ => Color::Gray,
            }),
        ),
        Span::raw(" "),
        Span::raw(
            doc.timestamp
                .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default(),
        ),
        Span::raw("  "),
        Span::raw(doc.source.line_number.to_string()),
    ]));
    for line in doc.text.lines().take(16) {
        lines.push(Line::from(line.to_string()));
    }
    lines.push(Line::from(""));
}

fn push_recent_doc_preview_lines<'a>(lines: &mut Vec<Line<'a>>, doc: &'a MessageDoc) {
    lines.push(Line::from(vec![
        Span::styled("[user]", Style::default().fg(Color::Cyan)),
        Span::raw(" "),
        Span::raw(
            doc.timestamp
                .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default(),
        ),
        Span::raw("  "),
        Span::raw(doc.source.line_number.to_string()),
    ]));
    let message_lines = doc.text.lines().collect::<Vec<_>>();
    for line in message_lines.iter().take(5) {
        lines.push(Line::from(line.to_string()));
    }
    if message_lines.len() > 5 {
        lines.push(Line::from(vec![Span::styled(
            format!("... {} more lines", message_lines.len() - 5),
            Style::default().fg(Color::Yellow),
        )]));
    }
    lines.push(Line::from(""));
}

fn short_id(session_id: &str) -> String {
    if session_id.len() <= 12 {
        session_id.to_string()
    } else {
        session_id[..12].to_string()
    }
}

fn project_name(cwd: Option<&Path>) -> String {
    cwd.and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("-")
        .to_string()
}

fn compact_status_text(input: &str) -> String {
    let max_chars = 32;
    let mut compact = input.replace('\n', " ");
    if compact.chars().count() > max_chars {
        compact = compact.chars().take(max_chars).collect::<String>();
        compact.push_str("...");
    }
    compact
}
