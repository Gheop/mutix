//! mutix-gtk: guided training and testing for mutix.
//!
//! Training is a karaoke: one word at a time is highlighted, you mouth it
//! without a sound, it turns green once its movement is recorded. The app
//! then measures how well the words are told apart, and a test screen shows
//! each word you mouth as it is recognized. A word said aloud shows in
//! orange: in daily use it would not be typed.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};

use mutix::config::{self, Config};
use mutix::dataset::{self, Dataset, Template};
use mutix::eval::{self, Report};
use mutix::mic::Microphone;
use mutix::model::{Decision, Model};
use mutix::segmenter::{self, Segmenter};
use mutix::sensor::{Event, Sensor};
use mutix::vocab::{self, Vocabulary};

const APP_ID: &str = "io.github.gheop.Mutix";
/// Time to see the next word before its capture starts.
const LEAD: Duration = Duration::from_millis(600);
/// Capture window per word; the features keep only the part that moves.
const WINDOW: Duration = Duration::from_millis(2300);
const RETRIES: usize = 2;
/// A segment with this much voice in it was said aloud.
const VOICED: f32 = 0.25;

const CSS: &str = "
.karaoke { font-size: 30pt; font-weight: 700; }
.chip { padding: 6px 16px; border-radius: 999px; font-size: 18pt; font-weight: 700; }
.chip.silent { background: alpha(@success_color, 0.18); color: @success_color; }
.chip.rejected { background: alpha(@view_fg_color, 0.07); color: alpha(@view_fg_color, 0.45); }
.chip.voiced { background: alpha(@warning_color, 0.2); color: @warning_color; }
.hint { font-size: 14pt; }
";

fn french() -> bool {
    static FR: OnceLock<bool> = OnceLock::new();
    *FR.get_or_init(|| {
        std::env::var("LANGUAGE")
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default()
            .starts_with("fr")
    })
}

/// French or English text, after the session locale.
fn t(fr: &'static str, en: &'static str) -> &'static str {
    if french() { fr } else { en }
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Pending,
    Done,
    Missed,
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Lead,
    Capture,
}

struct Karaoke {
    lines: Vec<Vec<String>>,
    marks: Vec<Vec<Mark>>,
    line: usize,
    word: usize,
    stage: Stage,
    since: Instant,
    retries: usize,
    paused: bool,
    words: Vec<String>,
}

enum Phase {
    Welcome,
    /// Measuring the still noise, then going on.
    Still {
        started: Instant,
        then: Next,
    },
    Karaoke(Karaoke),
    Evaluating(Receiver<anyhow::Result<Report>>, Vec<String>),
    Results,
    Test,
}

#[derive(Clone, Copy, PartialEq)]
enum Next {
    Karaoke,
    Test,
}

struct Widgets {
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
    title: adw::WindowTitle,
    device_row: adw::ActionRow,
    mic_row: adw::ActionRow,
    lang_row: adw::ComboRow,
    reps_row: adw::SpinRow,
    fresh_row: adw::SwitchRow,
    test_button: gtk::Button,
    still_progress: gtk::ProgressBar,
    karaoke: gtk::Label,
    karaoke_hint: gtk::Label,
    karaoke_progress: gtk::ProgressBar,
    karaoke_count: gtk::Label,
    pause: gtk::Button,
    results_page: adw::StatusPage,
    results_list: gtk::Box,
    weak_button: gtk::Button,
    test_flow: gtk::FlowBox,
    test_hint: gtk::Label,
    test_motion: gtk::LevelBar,
    test_voice: gtk::Label,
}

struct App {
    w: Widgets,
    cfg: Config,
    dataset_path: PathBuf,
    ds: Dataset,
    sensor: Sensor,
    seg: Segmenter,
    mic: Option<Microphone>,
    connected: bool,
    rate: (Instant, usize, f32),
    model: Option<Model>,
    model_rx: Option<Receiver<anyhow::Result<Model>>>,
    weak: Vec<String>,
    vocabulary: &'static Vocabulary,
    phase: Phase,
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[])
}

fn build(gtk_app: &adw::Application) {
    gtk::Window::set_default_icon_name(APP_ID);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let cfg = Config::load(&config::default_path()).unwrap_or_default();
    let w = widgets();
    let window = adw::ApplicationWindow::builder()
        .application(gtk_app)
        .title("Mutix")
        .default_width(780)
        .default_height(700)
        .content(&toolbar(&w))
        .build();

    let mic = match Microphone::open(cfg.microphone.as_deref()) {
        Ok(m) => {
            w.mic_row.set_subtitle(t(
                "Une parole à voix haute s'affiche en orange et n'est pas tapée",
                "Words said aloud show in orange and are not typed",
            ));
            Some(m)
        }
        Err(e) => {
            w.mic_row
                .set_subtitle(&format!("{}: {e:#}", t("Indisponible", "Unavailable")));
            None
        }
    };
    let locale = std::env::var("LANG").unwrap_or_default();
    let vocabulary = Vocabulary::for_locale(&locale);
    w.lang_row
        .set_selected(vocab::ALL.iter().position(|v| v.code == vocabulary.code).unwrap_or(0) as u32);

    let dataset_path = dataset::default_path();
    let ds = Dataset::load(&dataset_path).unwrap_or_default();
    w.test_button.set_sensitive(!ds.templates.is_empty());
    let app = Rc::new(RefCell::new(App {
        sensor: Sensor::spawn(cfg.device.clone(), cfg.interval_us()),
        seg: Segmenter::new(segmenter::Params::for_rate(cfg.rate_hz(), cfg.trigger())),
        w,
        cfg,
        dataset_path,
        ds,
        mic,
        connected: false,
        rate: (Instant::now(), 0, 0.0),
        model: None,
        model_rx: None,
        weak: Vec::new(),
        vocabulary,
        phase: Phase::Welcome,
    }));
    connect(&app);

    let ticker = app.clone();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        ticker.borrow_mut().tick();
        glib::ControlFlow::Continue
    });
    window.present();
    // Enter starts the training; nothing else should look focused at start.
    if let Some(start) = named_button(&window.clone().upcast(), "start") {
        window.set_default_widget(Some(&start));
        start.grab_focus();
    }
}

fn page(child: &impl IsA<gtk::Widget>) -> adw::Clamp {
    adw::Clamp::builder()
        .maximum_size(640)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(16)
        .margin_end(16)
        .child(child)
        .build()
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

fn pill(label: &str, suggested: bool) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.add_css_class("pill");
    if suggested {
        b.add_css_class("suggested-action");
    }
    b.set_halign(gtk::Align::Center);
    b
}

fn widgets() -> Widgets {
    let lang_names: Vec<&str> = vocab::ALL.iter().map(|v| v.name).collect();
    let reps_row = adw::SpinRow::with_range(4.0, 15.0, 1.0);
    reps_row.set_title(t("Répétitions par mot", "Takes per word"));
    reps_row.set_value(8.0);
    let w = Widgets {
        stack: gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build(),
        toasts: adw::ToastOverlay::new(),
        title: adw::WindowTitle::new("Mutix", t("Recherche des AirPods…", "Looking for AirPods…")),
        device_row: adw::ActionRow::builder()
            .title("AirPods")
            .subtitle(t("Recherche…", "Looking…"))
            .build(),
        mic_row: adw::ActionRow::builder().title(t("Micro", "Microphone")).build(),
        lang_row: adw::ComboRow::builder()
            .title(t("Vocabulaire", "Vocabulary"))
            .model(&gtk::StringList::new(&lang_names))
            .build(),
        reps_row,
        fresh_row: adw::SwitchRow::builder()
            .title(t("Repartir de zéro", "Start from scratch"))
            .subtitle(t(
                "Oublie les prises déjà enregistrées pour ces mots",
                "Forget the takes already recorded for these words",
            ))
            .build(),
        test_button: pill(t("Tester", "Test"), false),
        still_progress: gtk::ProgressBar::new(),
        karaoke: gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["karaoke"])
            .build(),
        karaoke_hint: gtk::Label::builder().css_classes(["hint", "dim-label"]).build(),
        karaoke_progress: gtk::ProgressBar::new(),
        karaoke_count: gtk::Label::builder().css_classes(["dim-label"]).build(),
        pause: pill(t("Pause", "Pause"), false),
        results_page: adw::StatusPage::new(),
        results_list: vbox(6),
        weak_button: pill(t("Réentraîner les mots faibles", "Train the weak words again"), false),
        test_flow: gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(6)
            .row_spacing(10)
            .column_spacing(10)
            .valign(gtk::Align::Start)
            .build(),
        test_hint: gtk::Label::builder().css_classes(["hint"]).build(),
        test_motion: gtk::LevelBar::for_interval(0.0, 3.0),
        test_voice: gtk::Label::builder().css_classes(["dim-label"]).build(),
    };
    w.stack.add_named(&welcome_page(&w), Some("welcome"));
    w.stack.add_named(&still_page(&w), Some("still"));
    w.stack.add_named(&karaoke_page(&w), Some("karaoke"));
    w.stack.add_named(&results_page(&w), Some("results"));
    w.stack.add_named(&test_page(&w), Some("test"));
    w
}

fn toolbar(w: &Widgets) -> adw::ToolbarView {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&w.title));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    w.toasts.set_child(Some(&w.stack));
    view.set_content(Some(&w.toasts));
    view
}

/// A page whose content scrolls above a bar of buttons that stays visible,
/// whatever the window height.
fn with_buttons(content: &impl IsA<gtk::Widget>, buttons: &gtk::Box) -> adw::ToolbarView {
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(12);
    buttons.set_margin_bottom(12);
    let view = adw::ToolbarView::new();
    view.set_content(Some(&gtk::ScrolledWindow::builder().child(content).vexpand(true).build()));
    view.add_bottom_bar(buttons);
    view
}

fn welcome_page(w: &Widgets) -> adw::ToolbarView {
    let status = adw::StatusPage::builder()
        .title("Mutix")
        .description(t(
            "Apprenez à mutix les mots que vous articulez en silence. Un mot s'allume, vous le formez avec la bouche sans faire de son, il passe au vert. Environ 5 minutes pour 12 mots.",
            "Teach mutix the words you mouth silently. A word lights up, you shape it with your mouth without a sound, it turns green. About 5 minutes for 12 words.",
        ))
        .build();
    status.add_css_class("compact");
    match gdk::Texture::from_bytes(&glib::Bytes::from_static(include_bytes!("../../assets/icon.svg"))) {
        Ok(tex) => status.set_paintable(Some(&tex)),
        Err(_) => status.set_icon_name(Some("audio-headphones-symbolic")),
    }
    let devices = adw::PreferencesGroup::new();
    devices.add(&w.device_row);
    devices.add(&w.mic_row);
    let options = adw::PreferencesGroup::new();
    options.add(&w.lang_row);
    options.add(&w.reps_row);
    options.add(&w.fresh_row);

    let start = pill(t("Commencer l'entraînement", "Start training"), true);
    start.set_widget_name("start");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.append(&start);
    buttons.append(&w.test_button);

    let col = vbox(24);
    col.append(&devices);
    col.append(&options);
    status.set_child(Some(&page(&col)));
    with_buttons(&status, &buttons)
}

fn still_page(w: &Widgets) -> adw::StatusPage {
    let col = vbox(12);
    col.append(&w.still_progress);
    adw::StatusPage::builder()
        .icon_name("face-plain-symbolic")
        .title(t("Ne bougez plus", "Keep still"))
        .description(t(
            "Bouche fermée, tête immobile : mutix mesure le bruit des capteurs pendant 2 secondes.",
            "Mouth closed, head still: mutix measures the sensor noise for 2 seconds.",
        ))
        .child(&page(&col))
        .build()
}

fn karaoke_page(w: &Widgets) -> gtk::Box {
    let col = vbox(28);
    col.set_valign(gtk::Align::Center);
    let title = gtk::Label::builder()
        .label(t(
            "Articulez en silence le mot surligné",
            "Mouth the highlighted word without a sound",
        ))
        .css_classes(["title-2"])
        .build();
    col.append(&title);
    col.append(&w.karaoke);
    col.append(&w.karaoke_progress);
    col.append(&w.karaoke_hint);
    col.append(&w.karaoke_count);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    let skip = pill(t("Passer ce mot", "Skip this word"), false);
    skip.set_widget_name("skip");
    let stop = pill(t("Arrêter", "Stop"), false);
    stop.set_widget_name("stop-training");
    buttons.append(&w.pause);
    buttons.append(&skip);
    buttons.append(&stop);
    col.append(&buttons);
    let outer = vbox(0);
    outer.set_vexpand(true);
    outer.append(&page(&col));
    outer.set_valign(gtk::Align::Center);
    outer
}

fn results_page(w: &Widgets) -> adw::ToolbarView {
    let col = vbox(18);
    col.append(&w.results_list);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    let test = pill(t("Tester maintenant", "Test now"), true);
    test.set_widget_name("results-test");
    let home = pill(t("Accueil", "Home"), false);
    home.set_widget_name("results-home");
    buttons.append(&test);
    buttons.append(&w.weak_button);
    buttons.append(&home);
    let note = gtk::Label::builder()
        .label(t(
            "Pour l'utiliser partout : lancez « mutix listen » dans un terminal.",
            "To use it everywhere: run \"mutix listen\" in a terminal.",
        ))
        .css_classes(["dim-label"])
        .wrap(true)
        .build();
    col.append(&note);
    w.results_page.set_child(Some(&page(&col)));
    with_buttons(&w.results_page, &buttons)
}

fn test_page(w: &Widgets) -> gtk::Box {
    let col = vbox(16);
    w.test_hint.set_label(t(
        "Articulez des mots du vocabulaire, en silence. Essayez aussi d'en dire un à voix haute.",
        "Mouth words from the vocabulary, silently. Try saying one aloud too.",
    ));
    w.test_hint.set_wrap(true);
    col.append(&w.test_hint);
    let meters = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let motion = gtk::Label::builder()
        .label(t("Mouvement", "Motion"))
        .css_classes(["dim-label"])
        .build();
    w.test_motion.set_hexpand(true);
    meters.append(&motion);
    meters.append(&w.test_motion);
    meters.append(&w.test_voice);
    col.append(&meters);
    let legend = gtk::Label::builder()
        .use_markup(true)
        .label(t(
            "<span foreground='#26a269'>vert</span> : reconnu · <span foreground='#888888'>gris</span> : pas sûr · <span foreground='#e5a50a'>orange</span> : dit à voix haute, ignoré",
            "<span foreground='#26a269'>green</span>: recognized · <span foreground='#888888'>grey</span>: unsure · <span foreground='#e5a50a'>orange</span>: said aloud, ignored",
        ))
        .build();
    col.append(&legend);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&w.test_flow)
        .vexpand(true)
        .min_content_height(240)
        .build();
    col.append(&scroll);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    let clear = pill(t("Effacer", "Clear"), false);
    clear.set_widget_name("test-clear");
    let home = pill(t("Accueil", "Home"), false);
    home.set_widget_name("test-home");
    buttons.append(&clear);
    buttons.append(&home);
    col.append(&buttons);
    let outer = vbox(0);
    outer.append(&page(&col));
    outer
}

/// Finds a button by the name given when it was built.
fn named_button(root: &gtk::Widget, name: &str) -> Option<gtk::Button> {
    if root.widget_name() == name
        && let Ok(b) = root.clone().downcast::<gtk::Button>()
    {
        return Some(b);
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(b) = named_button(&c, name) {
            return Some(b);
        }
        child = c.next_sibling();
    }
    None
}

fn connect(app: &Rc<RefCell<App>>) {
    let root: gtk::Widget = app.borrow().w.stack.clone().upcast();
    let on = |name: &str, f: fn(&mut App)| {
        if let Some(b) = named_button(&root, name) {
            let app = app.clone();
            b.connect_clicked(move |_| f(&mut app.borrow_mut()));
        }
    };
    on("start", App::start_training);
    on("skip", App::skip_word);
    on("stop-training", App::stop_training);
    on("results-test", App::start_test);
    on("results-home", App::go_home);
    on("test-clear", App::clear_test);
    on("test-home", App::go_home);
    let a = app.clone();
    app.borrow()
        .w
        .test_button
        .connect_clicked(move |_| a.borrow_mut().start_test());
    let a = app.clone();
    app.borrow()
        .w
        .pause
        .connect_clicked(move |_| a.borrow_mut().toggle_pause());
    let a = app.clone();
    app.borrow()
        .w
        .weak_button
        .connect_clicked(move |_| a.borrow_mut().train_weak());
}

impl App {
    fn toast(&self, msg: &str) {
        self.w.toasts.add_toast(adw::Toast::new(msg));
    }

    fn show(&self, name: &str) {
        self.w.stack.set_visible_child_name(name);
    }

    fn go_home(&mut self) {
        self.seg.cancel();
        self.seg.auto = false;
        self.phase = Phase::Welcome;
        self.w.test_button.set_sensitive(!self.ds.templates.is_empty());
        self.show("welcome");
    }

    fn require_airpods(&self) -> bool {
        if !self.connected {
            self.toast(t(
                "Les AirPods ne sont pas connectés, ou pas dans vos oreilles",
                "The AirPods are not connected, or not in your ears",
            ));
        }
        self.connected
    }

    fn start_training(&mut self) {
        if !self.require_airpods() {
            return;
        }
        let vocabulary = vocab::ALL[(self.w.lang_row.selected() as usize).min(vocab::ALL.len() - 1)];
        self.vocabulary = vocabulary;
        let words: Vec<String> = vocabulary.words().iter().map(|s| s.to_string()).collect();
        if self.w.fresh_row.is_active() {
            for word in &words {
                self.ds.remove_word(word);
            }
            let _ = self.ds.save(&self.dataset_path);
        }
        let reps = self.w.reps_row.value() as usize;
        self.begin(words, reps);
    }

    fn train_weak(&mut self) {
        if self.weak.is_empty() || !self.require_airpods() {
            return;
        }
        let words = self.weak.clone();
        self.begin(words, 6);
    }

    fn begin(&mut self, words: Vec<String>, reps: usize) {
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        let lines = vocab::lines(&refs, reps, 4, seed);
        let marks = lines.iter().map(|l| vec![Mark::Pending; l.len()]).collect();
        let karaoke = Karaoke {
            lines,
            marks,
            line: 0,
            word: 0,
            stage: Stage::Lead,
            since: Instant::now(),
            retries: 0,
            paused: false,
            words,
        };
        self.model = None;
        self.calibrate_then(Next::Karaoke);
        // Kept aside until the still phase ends.
        self.pending_karaoke(karaoke);
    }

    fn pending_karaoke(&mut self, k: Karaoke) {
        PENDING.with(|p| *p.borrow_mut() = Some(k));
    }

    fn calibrate_then(&mut self, then: Next) {
        self.seg.cancel();
        self.seg.auto = false;
        self.seg.calibrate();
        self.w.still_progress.set_fraction(0.0);
        self.phase = Phase::Still {
            started: Instant::now(),
            then,
        };
        self.show("still");
    }

    fn stop_training(&mut self) {
        let words = match &self.phase {
            Phase::Karaoke(k) => k.words.clone(),
            _ => Vec::new(),
        };
        self.seg.cancel();
        self.evaluate(words);
    }

    fn skip_word(&mut self) {
        if let Phase::Karaoke(k) = &mut self.phase {
            k.marks[k.line][k.word] = Mark::Missed;
            self.seg.cancel();
            self.advance();
        }
    }

    fn toggle_pause(&mut self) {
        if let Phase::Karaoke(k) = &mut self.phase {
            k.paused = !k.paused;
            k.stage = Stage::Lead;
            k.since = Instant::now();
            self.seg.cancel();
            self.w.pause.set_label(if k.paused {
                t("Reprendre", "Resume")
            } else {
                t("Pause", "Pause")
            });
            self.render_karaoke();
        }
    }

    fn start_test(&mut self) {
        if !self.require_airpods() {
            return;
        }
        if self.ds.templates.is_empty() {
            self.toast(t("Entraînez d'abord quelques mots", "Train a few words first"));
            return;
        }
        if self.model.is_none() && self.model_rx.is_none() {
            let takes = self.ds.templates.clone();
            let params = self.cfg.params().unwrap_or_default();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || tx.send(Model::train(&takes, params)));
            self.model_rx = Some(rx);
        }
        self.calibrate_then(Next::Test);
    }

    fn clear_test(&mut self) {
        self.w.test_flow.remove_all();
    }

    fn evaluate(&mut self, words: Vec<String>) {
        let takes: Vec<Template> = if words.is_empty() {
            self.ds.templates.clone()
        } else {
            self.ds
                .templates
                .iter()
                .filter(|t| words.contains(&t.word))
                .cloned()
                .collect()
        };
        let params = self.cfg.params().unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(eval::evaluate(&takes, params.set, params.band, 300)));
        self.w.results_page.set_title(t("Calcul…", "Computing…"));
        self.w.results_page.set_description(None);
        clear_box(&self.w.results_list);
        self.w.weak_button.set_visible(false);
        self.phase = Phase::Evaluating(rx, words);
        self.show("results");
    }

    fn tick(&mut self) {
        while let Ok(ev) = self.sensor.rx.try_recv() {
            self.on_sensor(ev);
        }
        if let Some(rx) = &self.model_rx
            && let Ok(result) = rx.try_recv()
        {
            self.model_rx = None;
            match result {
                Ok(m) => self.model = Some(m),
                Err(e) => self.toast(&format!("{e:#}")),
            }
        }
        if let Phase::Still { started, .. } = &self.phase {
            let elapsed = started.elapsed();
            self.w
                .still_progress
                .set_fraction((elapsed.as_secs_f64() / 2.0).min(1.0));
            if elapsed > Duration::from_secs(8) {
                self.toast(t("Aucune donnée des AirPods", "No data from the AirPods"));
                self.go_home();
            }
        }
        if matches!(self.phase, Phase::Karaoke(_)) {
            self.karaoke_tick();
        }
        let evaluated = match &self.phase {
            Phase::Evaluating(rx, _) => rx.try_recv().ok(),
            _ => None,
        };
        if let Some(result) = evaluated {
            let words = match std::mem::replace(&mut self.phase, Phase::Results) {
                Phase::Evaluating(_, w) => w,
                _ => Vec::new(),
            };
            self.show_results(result, &words);
        }
        if matches!(self.phase, Phase::Test) {
            self.w.test_motion.set_value(f64::from(
                self.seg.energy / self.seg.threshold().unwrap_or(3.0).max(0.1),
            ));
            let speaking = self.mic.as_ref().is_some_and(Microphone::speaking);
            self.w.test_voice.set_label(if speaking {
                t("🗣 voix", "🗣 voice")
            } else {
                t("silence", "silent")
            });
        }
    }

    fn on_sensor(&mut self, ev: Event) {
        match ev {
            Event::Connecting(name) => self
                .w
                .device_row
                .set_subtitle(&format!("{} {name}…", t("Connexion à", "Connecting to"))),
            Event::Connected { name, .. } => {
                self.connected = true;
                self.w.device_row.set_subtitle(&name);
                self.w.title.set_subtitle(&name);
            }
            Event::Lost(why) => {
                self.connected = false;
                let text = if why.contains("no AirPods connected") {
                    t(
                        "Non connectés : connectez-les dans les paramètres Bluetooth",
                        "Not connected: connect them in the Bluetooth settings",
                    )
                    .to_string()
                } else if why.contains("no motion data") {
                    t(
                        "Connectés, mais sans données : mettez les deux dans vos oreilles",
                        "Connected but silent: put both of them in your ears",
                    )
                    .to_string()
                } else {
                    why
                };
                self.w.device_row.set_subtitle(&text);
                self.w
                    .title
                    .set_subtitle(t("AirPods déconnectés", "AirPods disconnected"));
            }
            Event::Ears(l, r) => {
                if !(l && r) {
                    self.toast(t(
                        "Une oreillette est sortie : le flux reprendra quand les deux seront en place",
                        "A bud is out: the stream resumes once both are in",
                    ));
                }
            }
            Event::Frame(f) => {
                self.rate.1 += 1;
                if self.rate.0.elapsed() >= Duration::from_secs(1) {
                    self.rate.2 = self.rate.1 as f32 / self.rate.0.elapsed().as_secs_f32();
                    self.rate = (Instant::now(), 0, self.rate.2);
                    if self.connected {
                        let name = self.w.device_row.subtitle().map(|s| s.to_string()).unwrap_or_default();
                        let name = name.split(" · ").next().unwrap_or("").to_string();
                        self.w
                            .device_row
                            .set_subtitle(&format!("{name} · {:.0} Hz", self.rate.2));
                    }
                }
                if let Some(ev) = self.seg.push(f) {
                    self.on_segment(ev);
                }
            }
        }
    }

    fn on_segment(&mut self, ev: segmenter::Event) {
        let still = match &self.phase {
            Phase::Still { then, .. } => Some(*then),
            _ => None,
        };
        let testing = matches!(self.phase, Phase::Test);
        match (still, ev) {
            (Some(then), segmenter::Event::Calibrated) => match then {
                Next::Karaoke => {
                    if let Some(mut k) = PENDING.with(|p| p.borrow_mut().take()) {
                        k.since = Instant::now();
                        self.phase = Phase::Karaoke(k);
                        self.w.pause.set_label(t("Pause", "Pause"));
                        self.render_karaoke();
                        self.show("karaoke");
                    }
                }
                Next::Test => {
                    self.seg.auto = true;
                    self.phase = Phase::Test;
                    self.show("test");
                }
            },
            (None, segmenter::Event::Finished(frames)) if testing => self.test_segment(&frames),
            _ => {}
        }
    }

    /// True when the last `span` seconds held voice.
    fn voiced(&self, span: f64) -> bool {
        let now = Instant::now();
        let from = now.checked_sub(Duration::from_secs_f64(span.max(0.0))).unwrap_or(now);
        self.mic.as_ref().is_some_and(|m| m.voiced_share(from, now) > VOICED)
    }

    fn test_segment(&mut self, frames: &[mutix::frame::Frame]) {
        let Some(model) = &self.model else {
            self.toast(t("Le modèle se prépare…", "The model is getting ready…"));
            return;
        };
        let d: Decision = model.classify(frames);
        let span = frames.last().map(|l| l.t - frames[0].t).unwrap_or(0.0);
        let (text, class) = if self.voiced(span) {
            (format!("🗣 {}", d.best), "voiced")
        } else if let Some(w) = &d.word {
            (w.clone(), "silent")
        } else {
            (format!("{} ?", d.best), "rejected")
        };
        let chip = gtk::Label::builder().label(&text).css_classes(["chip", class]).build();
        self.w.test_flow.insert(&chip, 0);
        while let Some(last) = self.w.test_flow.child_at_index(40) {
            self.w.test_flow.remove(&last);
        }
    }

    fn karaoke_tick(&mut self) {
        let Phase::Karaoke(k) = &mut self.phase else { return };
        if k.paused {
            return;
        }
        let elapsed = k.since.elapsed();
        match k.stage {
            Stage::Lead => {
                self.w.karaoke_progress.set_fraction(0.0);
                if elapsed >= LEAD {
                    k.stage = Stage::Capture;
                    k.since = Instant::now();
                    self.seg.start();
                    self.w.karaoke_hint.set_label(t("Maintenant", "Now"));
                    self.render_karaoke();
                }
            }
            Stage::Capture => {
                self.w
                    .karaoke_progress
                    .set_fraction((elapsed.as_secs_f64() / WINDOW.as_secs_f64()).min(1.0));
                if elapsed >= WINDOW {
                    self.finish_capture();
                }
            }
        }
    }

    fn finish_capture(&mut self) {
        let ev = self.seg.stop();
        let peak = self.seg.peak;
        let k_thr = self.seg.params.k;
        let voiced = self.voiced(WINDOW.as_secs_f64());
        let Phase::Karaoke(k) = &mut self.phase else { return };
        let word = k.lines[k.line][k.word].clone();
        let frames = match ev {
            segmenter::Event::Finished(f) => f,
            _ => Vec::new(),
        };
        let retry = |k: &mut Karaoke, hint: &str, w: &Widgets| {
            w.karaoke_hint.set_label(hint);
            k.retries += 1;
            k.stage = Stage::Lead;
            k.since = Instant::now();
        };
        if voiced {
            if k.retries < RETRIES {
                retry(
                    k,
                    t(
                        "C'était à voix haute : en silence, s'il vous plaît",
                        "That was aloud: silently, please",
                    ),
                    &self.w,
                );
                self.render_karaoke();
                return;
            }
            k.marks[k.line][k.word] = Mark::Missed;
        } else if peak < k_thr || frames.is_empty() {
            if k.retries < RETRIES {
                retry(
                    k,
                    t(
                        "Je n'ai rien senti : un peu plus marqué",
                        "Nothing came through: a little wider",
                    ),
                    &self.w,
                );
                self.render_karaoke();
                return;
            }
            k.marks[k.line][k.word] = Mark::Missed;
        } else {
            k.marks[k.line][k.word] = Mark::Done;
            self.ds.add(&word, frames);
            if let Err(e) = self.ds.save(&self.dataset_path) {
                self.toast(&format!("{e:#}"));
            }
            self.w.karaoke_hint.set_label("✓");
        }
        self.advance();
    }

    fn advance(&mut self) {
        let Phase::Karaoke(k) = &mut self.phase else { return };
        k.retries = 0;
        k.stage = Stage::Lead;
        k.since = Instant::now();
        k.word += 1;
        if k.word >= k.lines[k.line].len() {
            k.word = 0;
            k.line += 1;
        }
        if k.line >= k.lines.len() {
            let words = k.words.clone();
            self.evaluate(words);
            return;
        }
        self.render_karaoke();
    }

    fn render_karaoke(&self) {
        let Phase::Karaoke(k) = &self.phase else { return };
        let line = &k.lines[k.line];
        let parts: Vec<String> = line
            .iter()
            .enumerate()
            .map(|(i, word)| {
                let word = glib::markup_escape_text(word);
                if i == k.word && !k.paused {
                    match k.stage {
                        Stage::Lead => format!("<u>{word}</u>"),
                        Stage::Capture => {
                            format!("<span background='#3584e4' foreground='#ffffff'>\u{2009}{word}\u{2009}</span>")
                        }
                    }
                } else {
                    match k.marks[k.line][i] {
                        Mark::Done => format!("<span foreground='#26a269'>{word}</span>"),
                        Mark::Missed => format!("<span foreground='#c01c28'><s>{word}</s></span>"),
                        Mark::Pending => format!("<span alpha='45%'>{word}</span>"),
                    }
                }
            })
            .collect();
        self.w.karaoke.set_markup(&parts.join("   "));
        let done: usize = k.marks.iter().flatten().filter(|m| **m != Mark::Pending).count();
        let total: usize = k.marks.iter().map(Vec::len).sum();
        self.w.karaoke_count.set_label(&format!(
            "{} {}/{} · {} {}/{}",
            t("Ligne", "Line"),
            k.line + 1,
            k.lines.len(),
            t("mot", "word"),
            done + 1,
            total
        ));
        if k.paused {
            self.w.karaoke_hint.set_label(t("En pause", "Paused"));
        } else if k.stage == Stage::Lead && k.retries == 0 {
            self.w.karaoke_hint.set_label(t("Prêt…", "Ready…"));
        }
    }

    fn show_results(&mut self, result: anyhow::Result<Report>, words: &[String]) {
        clear_box(&self.w.results_list);
        self.weak.clear();
        let r = match result {
            Ok(r) => r,
            Err(e) => {
                self.w
                    .results_page
                    .set_title(t("Pas encore assez de prises", "Not enough takes yet"));
                self.w.results_page.set_description(Some(&format!("{e:#}")));
                return;
            }
        };
        self.model = None;
        self.w.results_page.set_title(&format!(
            "{:.0} % {}",
            r.accuracy * 100.0,
            t("des prises reconnues", "of takes recognized")
        ));
        let verdict = if r.significant() {
            t(
                "Les AirPods distinguent bien vos mots.",
                "The AirPods tell your words apart.",
            )
        } else {
            t(
                "Pas mieux que le hasard pour l'instant : enregistrez plus de prises.",
                "No better than chance yet: record more takes.",
            )
        };
        self.w.results_page.set_description(Some(&format!(
            "{} {:.0} %. {verdict}",
            t("Au hasard :", "By chance:"),
            r.chance * 100.0
        )));
        let group = adw::PreferencesGroup::new();
        group.set_title(t("Par mot", "By word"));
        for (i, word) in r.words.iter().enumerate() {
            if !words.is_empty() && !words.contains(word) {
                continue;
            }
            let total: usize = r.confusion[i].iter().sum();
            let right = r.confusion[i][i];
            let share = right as f64 / total.max(1) as f64;
            let worst = r.confusion[i]
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .max_by_key(|(_, n)| **n)
                .filter(|(_, n)| **n > 0)
                .map(|(j, _)| r.words[j].clone());
            let mut subtitle = format!("{right}/{total} {}", t("reconnues", "recognized"));
            if let Some(w) = worst {
                subtitle.push_str(&format!(" · {} {w}", t("confondu avec", "mistaken for")));
            }
            let row = adw::ActionRow::builder().title(word).subtitle(&subtitle).build();
            let bar = gtk::LevelBar::for_interval(0.0, 1.0);
            bar.set_value(share);
            bar.set_width_request(120);
            bar.set_valign(gtk::Align::Center);
            row.add_suffix(&bar);
            group.add(&row);
            if share < 0.5 {
                self.weak.push(word.clone());
            }
        }
        self.w.results_list.append(&group);
        self.w.weak_button.set_visible(!self.weak.is_empty());
    }
}

fn clear_box(b: &gtk::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

thread_local! {
    static PENDING: RefCell<Option<Karaoke>> = const { RefCell::new(None) };
}
