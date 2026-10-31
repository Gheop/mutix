//! Terminal interface: live sensors, training with evaluation, and decoding.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Chart, Dataset as ChartData, Gauge, GraphType, List, ListItem, ListState, Paragraph, Row, Table, Tabs,
    Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::dataset::Dataset;
use crate::eval::{self, Report};
use crate::features::{FeatureSet, GROUPS};
use crate::frame;
use crate::model::{Decision, Model, Params};
use crate::segmenter::{self, Segmenter};
use crate::sensor::{Event, Sensor};

const HISTORY_SECONDS: f64 = 5.0;
const COLORS: [Color; 3] = [Color::Cyan, Color::Magenta, Color::Yellow];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Sensors,
    Train,
    Decode,
}

const SCREENS: [Screen; 3] = [Screen::Sensors, Screen::Train, Screen::Decode];

enum Job {
    Eval(Receiver<Result<Report>>),
    Model(Receiver<Result<Model>>),
}

struct App {
    cfg_params: Params,
    dataset_path: PathBuf,
    ds: Dataset,
    model: Option<Model>,
    model_stale: bool,
    job: Option<Job>,
    report: Option<Report>,
    set: FeatureSet,

    sensor: Sensor,
    status: String,
    connected: bool,
    ears: Option<(bool, bool)>,
    history: VecDeque<frame::Frame>,
    rate: f32,
    rate_window: (Instant, usize),

    seg: Segmenter,
    screen: Screen,
    selected: usize,
    input: Option<String>,
    extra_words: Vec<String>,
    confirm_delete: bool,
    train_auto: bool,
    decode_auto: bool,
    hold_keys: bool,
    last_take: Option<Vec<frame::Frame>>,
    transcript: VecDeque<Decision>,
    message: String,
    quit: bool,
}

pub fn run(cfg: &Config, dataset_path: &Path) -> Result<()> {
    let params = cfg.params()?;
    let ds = Dataset::load(dataset_path)?;
    let mut app = App {
        cfg_params: params,
        dataset_path: dataset_path.to_path_buf(),
        ds,
        model: None,
        model_stale: true,
        job: None,
        report: None,
        set: params.set,
        sensor: Sensor::spawn(cfg.device.clone(), cfg.interval_us()),
        status: "Looking for AirPods…".into(),
        connected: false,
        ears: None,
        history: VecDeque::new(),
        rate: 0.0,
        rate_window: (Instant::now(), 0),
        seg: Segmenter::new(segmenter::Params::for_rate(cfg.rate_hz(), cfg.trigger())),
        screen: Screen::Sensors,
        selected: 0,
        input: None,
        extra_words: Vec::new(),
        confirm_delete: false,
        train_auto: false,
        decode_auto: false,
        hold_keys: false,
        last_take: None,
        transcript: VecDeque::new(),
        message: String::new(),
        quit: false,
    };

    let mut terminal = ratatui::init();
    // Key release events let Space work as push-to-talk; terminals without
    // the kitty keyboard protocol fall back to press-to-start, press-to-stop.
    app.hold_keys = matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true))
        && execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        )
        .is_ok();
    let result = main_loop(&mut terminal, &mut app);
    if app.hold_keys {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}

fn main_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    while !app.quit {
        while let Ok(ev) = app.sensor.rx.try_recv() {
            app.on_sensor(ev)?;
        }
        app.poll_job();
        if last_draw.elapsed() >= Duration::from_millis(33) {
            terminal.draw(|f| draw(f, app))?;
            last_draw = Instant::now();
        }
        if event::poll(Duration::from_millis(10))?
            && let TermEvent::Key(key) = event::read()?
        {
            app.on_key(key)?;
        }
    }
    Ok(())
}

impl App {
    /// Recorded words plus the ones just added, which have no take yet.
    fn words(&self) -> Vec<(String, usize)> {
        let mut words = self.ds.words();
        for w in &self.extra_words {
            if !words.iter().any(|(x, _)| x == w) {
                words.push((w.clone(), 0));
            }
        }
        words.sort();
        words
    }

    fn selected_word(&self) -> Option<String> {
        self.words().get(self.selected).map(|(w, _)| w.clone())
    }

    fn on_sensor(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::Connecting(name) => self.status = format!("Connecting to {name}…"),
            Event::Connected { name, address } => {
                self.status = format!("{name} ({address})");
                self.connected = true;
                if self.seg.threshold().is_none() {
                    self.seg.calibrate();
                    self.message = "Measuring the still noise: keep still for 2 seconds.".into();
                }
            }
            Event::Lost(why) => {
                self.status = format!("{why}; retrying…");
                self.connected = false;
            }
            Event::Ears(l, r) => self.ears = Some((l, r)),
            Event::Frame(f) => {
                self.rate_window.1 += 1;
                let elapsed = self.rate_window.0.elapsed().as_secs_f32();
                if elapsed >= 1.0 {
                    self.rate = self.rate_window.1 as f32 / elapsed;
                    self.rate_window = (Instant::now(), 0);
                }
                self.history.push_back(f);
                while self.history.front().is_some_and(|h| f.t - h.t > HISTORY_SECONDS) {
                    self.history.pop_front();
                }
                self.seg.auto = match self.screen {
                    Screen::Train => self.train_auto && self.input.is_none(),
                    Screen::Decode => self.decode_auto,
                    Screen::Sensors => false,
                };
                if let Some(ev) = self.seg.push(f) {
                    self.on_segment(ev)?;
                }
            }
        }
        Ok(())
    }

    fn on_segment(&mut self, ev: segmenter::Event) -> Result<()> {
        match ev {
            segmenter::Event::Calibrated => {
                self.message = format!("Ready. Threshold {:.1}.", self.seg.threshold().unwrap_or(0.0));
            }
            segmenter::Event::Started => {}
            segmenter::Event::Dropped => self.message = "Too short to be a word; ignored.".into(),
            segmenter::Event::Finished(frames) => match self.screen {
                Screen::Train => {
                    let Some(word) = self.selected_word() else {
                        self.message = "Add a word first (n).".into();
                        return Ok(());
                    };
                    self.message = format!("Saved a take of \"{word}\" ({} samples).", frames.len());
                    self.last_take = Some(frames.clone());
                    self.ds.add(&word, frames);
                    self.ds.save(&self.dataset_path)?;
                    self.model_stale = true;
                }
                Screen::Decode => match &self.model {
                    Some(m) => {
                        let d = m.classify(&frames);
                        self.transcript.push_front(d);
                        self.transcript.truncate(200);
                    }
                    None => self.message = "No model yet; train some words first.".into(),
                },
                Screen::Sensors => {}
            },
        }
        Ok(())
    }

    fn poll_job(&mut self) {
        let done = match &self.job {
            Some(Job::Eval(rx)) => match rx.try_recv() {
                Ok(Ok(r)) => {
                    self.message = format!(
                        "{}: accuracy {:.0} %, chance {:.0} %, p = {:.3}.",
                        r.set.label(),
                        r.accuracy * 100.0,
                        r.chance * 100.0,
                        r.p_value
                    );
                    self.report = Some(r);
                    true
                }
                Ok(Err(e)) => {
                    self.message = format!("{e:#}");
                    true
                }
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => true,
            },
            Some(Job::Model(rx)) => match rx.try_recv() {
                Ok(Ok(m)) => {
                    self.message = format!(
                        "Model ready: {} takes, rejection distance {:.2}.",
                        m.templates.len(),
                        m.max_distance
                    );
                    self.model = Some(m);
                    true
                }
                Ok(Err(e)) => {
                    self.message = format!("{e:#}");
                    true
                }
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => true,
            },
            None => false,
        };
        if done {
            self.job = None;
        }
        if self.job.is_none() && self.screen == Screen::Decode && self.model_stale && !self.ds.templates.is_empty() {
            self.model_stale = false;
            let takes = self.ds.templates.clone();
            let params = Params {
                set: self.set,
                ..self.cfg_params
            };
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || tx.send(Model::train(&takes, params)));
            self.job = Some(Job::Model(rx));
            self.message = "Training the model…".into();
        }
    }

    fn start_eval(&mut self) {
        if self.job.is_some() {
            return;
        }
        let takes = self.ds.templates.clone();
        let (set, band) = (self.set, self.cfg_params.band);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(eval::evaluate(&takes, set, band, 500)));
        self.job = Some(Job::Eval(rx));
        self.message = format!("Evaluating {}…", set.label());
    }

    fn capture_key(&mut self, kind: KeyEventKind) {
        if self.screen == Screen::Train && self.selected_word().is_none() {
            self.message = "Add a word first (n).".into();
            return;
        }
        let stop = if self.hold_keys {
            kind == KeyEventKind::Release
        } else {
            self.seg.is_capturing()
        };
        if kind == KeyEventKind::Repeat {
            return;
        }
        if stop {
            if self.seg.is_capturing() {
                let ev = self.seg.stop();
                let _ = self.on_segment(ev);
            }
        } else if kind == KeyEventKind::Press {
            self.seg.start();
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(text) = self.input.as_mut() {
            if key.kind == KeyEventKind::Release {
                return Ok(());
            }
            match key.code {
                KeyCode::Enter => {
                    let word = text.trim().to_string();
                    self.input = None;
                    if !word.is_empty() {
                        if !self.extra_words.contains(&word) {
                            self.extra_words.push(word.clone());
                        }
                        self.selected = self.words().iter().position(|(w, _)| *w == word).unwrap_or(0);
                    }
                }
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            return Ok(());
        }

        if key.code == KeyCode::Char(' ') && self.screen != Screen::Sensors {
            self.capture_key(key.kind);
            return Ok(());
        }
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        if self.confirm_delete && key.code != KeyCode::Char('y') {
            self.confirm_delete = false;
            self.message = "Kept.".into();
            return Ok(());
        }
        let n_words = self.words().len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Tab => self.switch((self.screen_index() + 1) % 3),
            KeyCode::BackTab => self.switch((self.screen_index() + 2) % 3),
            KeyCode::Char('1') => self.switch(0),
            KeyCode::Char('2') => self.switch(1),
            KeyCode::Char('3') => self.switch(2),
            KeyCode::Char('c') => {
                self.seg.calibrate();
                self.message = "Measuring the still noise: keep still for 2 seconds.".into();
            }
            KeyCode::Char('a') => match self.screen {
                Screen::Train => {
                    self.train_auto = !self.train_auto;
                    self.message = if self.train_auto {
                        "Auto capture on: each movement is saved as a take."
                    } else {
                        "Auto capture off."
                    }
                    .into();
                }
                Screen::Decode => self.decode_auto = !self.decode_auto,
                Screen::Sensors => {}
            },
            KeyCode::Up if self.screen == Screen::Train => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down if self.screen == Screen::Train => {
                self.selected = (self.selected + 1).min(n_words.saturating_sub(1))
            }
            KeyCode::Char('n') if self.screen == Screen::Train => self.input = Some(String::new()),
            KeyCode::Char('x') if self.screen == Screen::Train => {
                if let Some(w) = self.selected_word()
                    && self.ds.remove_last(&w)
                {
                    self.ds.save(&self.dataset_path)?;
                    self.model_stale = true;
                    self.message = format!("Deleted the last take of \"{w}\".");
                    self.selected = self.selected.min(self.words().len().saturating_sub(1));
                }
            }
            KeyCode::Char('X') if self.screen == Screen::Train => {
                if let Some(w) = self.selected_word() {
                    self.confirm_delete = true;
                    self.message = format!("Delete every take of \"{w}\"? Press y to confirm.");
                }
            }
            KeyCode::Char('y') if self.confirm_delete => {
                self.confirm_delete = false;
                if let Some(w) = self.selected_word() {
                    let n = self.ds.remove_word(&w);
                    self.extra_words.retain(|x| *x != w);
                    self.ds.save(&self.dataset_path)?;
                    self.model_stale = true;
                    self.selected = self.selected.min(self.words().len().saturating_sub(1));
                    self.message = format!("Deleted {n} takes of \"{w}\".");
                }
            }
            KeyCode::Char('e') if self.screen == Screen::Train => self.start_eval(),
            KeyCode::Char(c @ ('o' | 'r' | 'l' | 'g')) if self.screen == Screen::Train => {
                let i = "orlg".find(c).unwrap_or(0);
                let mut flags = self.set.flags();
                flags[i] = !flags[i];
                let next = FeatureSet::from_flags(flags);
                if next.dims() > 0 {
                    self.set = next;
                    self.model_stale = true;
                    self.report = None;
                }
            }
            KeyCode::Char('C') if self.screen == Screen::Decode => self.transcript.clear(),
            _ => {}
        }
        Ok(())
    }

    fn screen_index(&self) -> usize {
        SCREENS.iter().position(|s| *s == self.screen).unwrap_or(0)
    }

    fn switch(&mut self, i: usize) {
        self.seg.cancel();
        self.screen = SCREENS[i];
    }
}

fn draw(f: &mut Frame, app: &App) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(8), Constraint::Length(3)]).areas(f.area());

    let titles = ["1 Sensors", "2 Train", "3 Decode"];
    let link = if app.connected {
        Span::styled(
            format!(" {}  {:.0} Hz ", app.status, app.rate),
            Style::new().fg(Color::Green),
        )
    } else {
        Span::styled(format!(" {} ", app.status), Style::new().fg(Color::Yellow))
    };
    let tabs = Tabs::new(titles)
        .select(app.screen_index())
        .highlight_style(Style::new().bold().fg(Color::Cyan))
        .block(
            Block::bordered()
                .title(" mutix ")
                .title(Line::from(link).right_aligned()),
        );
    f.render_widget(tabs, header);

    match app.screen {
        Screen::Sensors => draw_sensors(f, app, body),
        Screen::Train => draw_train(f, app, body),
        Screen::Decode => draw_decode(f, app, body),
    }

    let keys = match app.screen {
        Screen::Sensors => "Tab screens · c calibrate · q quit",
        Screen::Train if app.input.is_some() => "type the word · Enter add · Esc cancel",
        Screen::Train => {
            "↑↓ word · n new · Space capture · a auto · x delete take · X delete word · e evaluate · o r l g sensors · q quit"
        }
        Screen::Decode => "Space capture · a auto · c calibrate · C clear · q quit",
    };
    let mut lines = vec![Line::from(app.message.clone())];
    if let Some((l, r)) = app.ears
        && !(l && r)
    {
        lines =
            vec![Line::from("A bud is out of the ear: the motion stream pauses until both are in.").fg(Color::Yellow)];
    }
    let foot = Paragraph::new(lines).block(Block::bordered().title(Line::from(keys).dim()));
    f.render_widget(foot, footer);
}

fn energy_gauge(app: &App) -> Gauge<'_> {
    let thr = app.seg.threshold();
    let max = thr.map(|t| t * 3.0).unwrap_or(10.0).max(1.0);
    let ratio = (app.seg.energy / max).clamp(0.0, 1.0) as f64;
    let label = match thr {
        Some(t) => format!("motion {:.1} / threshold {:.1}", app.seg.energy, t),
        None if app.seg.is_calibrating() => "calibrating… keep still".to_string(),
        None => "not calibrated (c)".to_string(),
    };
    let loud = thr.is_some_and(|t| app.seg.energy > t);
    Gauge::default()
        .ratio(ratio)
        .label(label)
        .gauge_style(Style::new().fg(if loud { Color::Red } else { Color::Green }))
}

fn draw_sensors(f: &mut Frame, app: &App, area: Rect) {
    let [gauge, charts] = Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
    f.render_widget(energy_gauge(app), gauge);
    let rows = Layout::vertical([Constraint::Ratio(1, 2); 2]).split(charts);
    let mut cells = Vec::new();
    for r in rows.iter() {
        cells.extend(
            Layout::horizontal([Constraint::Ratio(1, 2); 2])
                .split(*r)
                .iter()
                .copied(),
        );
    }
    let Some(last) = app.history.back() else {
        f.render_widget(
            Paragraph::new("Waiting for motion samples…").block(Block::bordered()),
            charts,
        );
        return;
    };
    let t_end = last.t;
    let first_att = app.history.front().map(|h| h.attitude).unwrap_or_default();
    for (g, cell) in GROUPS.iter().zip(cells) {
        let series: Vec<Vec<(f64, f64)>> = (0..3)
            .map(|axis| {
                app.history
                    .iter()
                    .map(|h| {
                        let v = match *g {
                            "attitude" => f64::from(h.attitude[axis]) - f64::from(first_att[axis]),
                            "rotation" => f64::from(h.rotation[axis]),
                            "acceleration" => f64::from(h.acceleration[axis]),
                            _ => f64::from(h.gravity[axis]),
                        };
                        (h.t - t_end, v)
                    })
                    .collect()
            })
            .collect();
        let (lo, hi) = series
            .iter()
            .flatten()
            .fold((f64::MAX, f64::MIN), |(lo, hi), (_, v)| (lo.min(*v), hi.max(*v)));
        let pad = ((hi - lo) * 0.1).max(5.0);
        let datasets = series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                ChartData::default()
                    .name(["x", "y", "z"][i])
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::new().fg(COLORS[i]))
                    .data(s)
            })
            .collect();
        let title = match *g {
            "attitude" => "attitude (change)",
            "acceleration" => "user acceleration",
            other => other,
        };
        let chart = Chart::new(datasets)
            .block(Block::bordered().title(format!(" {title} ")))
            .x_axis(Axis::default().bounds([-HISTORY_SECONDS, 0.0]))
            .y_axis(
                Axis::default()
                    .bounds([lo - pad, hi + pad])
                    .labels([format!("{:.0}", lo - pad), format!("{:.0}", hi + pad)])
                    .style(Style::new().dim()),
            );
        f.render_widget(chart, cell);
    }
}

fn draw_train(f: &mut Frame, app: &App, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Length(30), Constraint::Min(30)]).areas(area);

    let words = app.words();
    let mut items: Vec<ListItem> = words
        .iter()
        .map(|(w, n)| ListItem::new(format!("{n:3}  {w}")))
        .collect();
    if let Some(text) = &app.input {
        items.push(ListItem::new(format!("  +  {text}▏")).fg(Color::Cyan));
    }
    let mut state =
        ListState::default().with_selected((!words.is_empty() && app.input.is_none()).then_some(app.selected));
    let list = List::new(items)
        .block(Block::bordered().title(" words (takes) "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, left, &mut state);

    let [top, gauge, report] =
        Layout::vertical([Constraint::Length(5), Constraint::Length(1), Constraint::Min(4)]).areas(right);
    let word = app.selected_word().unwrap_or_else(|| "—".into());
    let capture = if app.seg.is_capturing() {
        Span::styled("● capturing", Style::new().fg(Color::Red).bold())
    } else if app.train_auto {
        Span::styled("auto: mouth the word, then pause", Style::new().fg(Color::Green))
    } else if app.hold_keys {
        Span::raw("hold Space while you mouth the word")
    } else {
        Span::raw("Space to start, Space again to stop")
    };
    let sensors: Vec<Span> = GROUPS
        .iter()
        .zip(app.set.flags())
        .zip(["o", "r", "l", "g"])
        .flat_map(|((g, on), k)| {
            let style = if on {
                Style::new().fg(Color::Green)
            } else {
                Style::new().dim()
            };
            [Span::styled(format!("{k}:{g}{} ", if on { "✓" } else { "✗" }), style)]
        })
        .collect();
    let info = Paragraph::new(vec![
        Line::from(vec![Span::raw("Word: "), Span::styled(word, Style::new().bold())]),
        Line::from(capture),
        Line::from(sensors),
    ])
    .block(Block::bordered().title(" capture "));
    f.render_widget(info, top);
    f.render_widget(energy_gauge(app), gauge);

    let block = Block::bordered().title(" evaluation (e) ");
    match &app.report {
        Some(r) => {
            let verdict = if r.significant() {
                Span::styled("the motion tells these words apart", Style::new().fg(Color::Green))
            } else {
                Span::styled("not better than chance yet", Style::new().fg(Color::Yellow))
            };
            let head = vec![
                Line::from(format!(
                    "{} · {} takes · accuracy {:.0} % · chance {:.0} % · p = {:.3} ({} shuffles)",
                    r.set.label(),
                    r.takes,
                    r.accuracy * 100.0,
                    r.chance * 100.0,
                    r.p_value,
                    r.permutations
                )),
                Line::from(verdict),
                Line::from("Confusion (rows: said, columns: recognized)").dim(),
            ];
            let inner = block.inner(report);
            f.render_widget(block, report);
            let [h, t] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(inner);
            f.render_widget(Paragraph::new(head), h);
            let width = r
                .words
                .iter()
                .map(|w| w.chars().count())
                .max()
                .unwrap_or(4)
                .clamp(4, 14) as u16;
            let mut header = vec![String::new()];
            header.extend(r.words.iter().cloned());
            let rows = r.confusion.iter().enumerate().map(|(i, row)| {
                let mut cells = vec![r.words[i].clone()];
                cells.extend(row.iter().map(|v| v.to_string()));
                Row::new(cells)
            });
            let widths = vec![Constraint::Length(width + 1); r.words.len() + 1];
            f.render_widget(Table::new(rows, widths).header(Row::new(header).bold()), t);
        }
        None => {
            let text = "Record 5 to 10 takes of 2 or more words, then press e. The evaluation compares \
                        each take with all the others and checks the result against shuffled labels: \
                        p < 0.05 means the AirPods' motion really tells your words apart.";
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block), report);
        }
    }
}

fn draw_decode(f: &mut Frame, app: &App, area: Rect) {
    let [top, gauge, list] =
        Layout::vertical([Constraint::Length(5), Constraint::Length(1), Constraint::Min(3)]).areas(area);
    let mode = if app.seg.is_capturing() {
        Span::styled("● capturing", Style::new().fg(Color::Red).bold())
    } else if app.decode_auto {
        Span::styled(
            "auto: words are detected as you mouth them",
            Style::new().fg(Color::Green),
        )
    } else if app.hold_keys {
        Span::raw("hold Space while you mouth a word (a for auto)")
    } else {
        Span::raw("Space to start, Space again to stop (a for auto)")
    };
    let last = match app.transcript.front() {
        Some(Decision { word: Some(w), .. }) => Span::styled(w.clone(), Style::new().bold().fg(Color::Green)),
        Some(d) => Span::styled(format!("? (closest: {})", d.best), Style::new().fg(Color::Yellow)),
        None => Span::raw("—"),
    };
    let model = match &app.model {
        Some(m) => format!(
            "{} takes · {} · reject above {:.2}",
            m.templates.len(),
            m.params.set.label(),
            m.max_distance
        ),
        None => "no model yet".into(),
    };
    let info = Paragraph::new(vec![
        Line::from(mode),
        Line::from(vec![Span::raw("Last: "), last]),
        Line::from(model).dim(),
    ])
    .block(Block::bordered().title(" decode "));
    f.render_widget(info, top);
    f.render_widget(energy_gauge(app), gauge);

    let text: Vec<String> = app.transcript.iter().rev().filter_map(|d| d.word.clone()).collect();
    let rows = app.transcript.iter().map(|d| {
        let (word, style) = match &d.word {
            Some(w) => (w.clone(), Style::new().fg(Color::Green)),
            None => (format!("? {}", d.best), Style::new().fg(Color::Yellow)),
        };
        Row::new(vec![
            word,
            format!("{:.2}", d.distance),
            format!("{:.2}", d.margin),
            d.reason.unwrap_or("").to_string(),
        ])
        .style(style)
    });
    let [transcript, table] = Layout::vertical([Constraint::Length(3), Constraint::Min(2)]).areas(list);
    f.render_widget(
        Paragraph::new(text.join(" "))
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(" transcript ")),
        transcript,
    );
    let widths = [
        Constraint::Length(20),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Min(10),
    ];
    let table_widget = Table::new(rows, widths)
        .header(Row::new(["word", "distance", "margin", "rejected because"]).bold())
        .block(Block::bordered().title(" results "));
    f.render_widget(table_widget, table);
}
