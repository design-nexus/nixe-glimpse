use crate::player::Player;
use gtk::{gdk, gio, glib, prelude::*};
use crate::pw;
use nixe_glimpse::{
    config::{self, Paths},
    index, passwords, plugins, query, semantic, utility,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
/// The launcher keeps one width; it only grows downwards once there are results.
const WIDTH: i32 = 760;
const COLLAPSED: i32 = 56;
const EXPANDED: i32 = 460;
const LIST_WIDTH: i32 = 300;
thread_local! {static PANEL:RefCell<Option<gtk::ApplicationWindow>>=const{RefCell::new(None)};}
#[derive(Clone)]
enum Action {
    App(String),
    File(PathBuf, Value),
    Copy(String),
    System(&'static str),
    Plugin(String, String, plugins::Action),
    /// A Nixe Passwords row (`pw …`): an item's id and metadata, never a secret.
    Password(pw::Pw),
}
#[derive(Clone)]
struct ResultRow {
    title: String,
    /// The second line: a file's folder, a plugin's subtitle, a conversion's source.
    folder: String,
    /// Why it matched, shown as a small tag: "Related", "p. 4", "Line 12".
    reason: String,
    action: Action,
}
impl ResultRow {
    fn category(&self) -> (u8, &'static str) {
        match self.action {
            Action::App(_) => (0, "Applications"),
            Action::File(_, ref hit) if hit["related"] == true => (RELATED, "Related"),
            Action::File(_, _) => (1, "Files"),
            Action::Copy(_) => (2, "Utilities"),
            Action::System(_) => (3, "System"),
            Action::Plugin(_, _, _) => (4, "Plugins"),
            Action::Password(_) => (0, "Passwords"),
        }
    }
    fn identity(&self) -> String {
        match &self.action {
            Action::App(id) => format!("app:{id}"),
            Action::File(path, _) => format!("file:{}", path.display()),
            Action::Copy(value) => format!("copy:{value}"),
            Action::System(cmd) => format!("system:{cmd}"),
            Action::Plugin(id, hit, _) => format!("plugin:{id}:{hit}"),
            Action::Password(p) => p.key(),
        }
    }
}
/// A file's folder for display, with the home folder as "~".
fn folder_of(path: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    match std::env::var_os("HOME").map(PathBuf::from).and_then(|home| parent.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => parent.display().to_string(),
    }
}
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.).round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}
/// Where a content hit landed, in words: a page, a line or a moment.
fn match_place(hit: &Value) -> Option<String> {
    let loc = hit["loc"].as_f64();
    if let Some(page) = hit["info"]["page"].as_u64().or_else(|| loc.filter(|_| hit["kind"] == "pdf").map(|n| n as u64)) {
        return Some(format!("Page {page}"));
    }
    if let Some(line) = hit["info"]["line"].as_u64().or_else(|| loc.filter(|_| hit["segment"] == "lines").map(|n| n as u64)) {
        return Some(format!("Line {line}"));
    }
    hit["info"]["seconds"]
        .as_f64()
        .or_else(|| loc.filter(|_| hit["kind"] == "audio" || hit["kind"] == "video"))
        .map(|s| format!("At {}", clock(s)))
}
/// The tag beside a file found by its contents rather than its name.
fn match_reason(hit: &Value) -> String {
    if hit.is_null() {
        return String::new();
    }
    if hit["related"] == true {
        return "Related".into();
    }
    match match_place(hit) {
        Some(place) => place.replace("Page ", "p. "),
        None => "Content".into(),
    }
}
fn focus_search(window: &gtk::ApplicationWindow, entry: &gtk::SearchEntry) {
    gtk::prelude::GtkWindowExt::set_focus(window, Some(entry));
    entry.grab_focus();
    let entry = entry.clone();
    glib::idle_add_local_once(move || {
        entry.grab_focus();
    });
}
struct Reply {
    generation: u64,
    rows: Vec<ResultRow>,
    append: bool,
    status: String,
}
pub fn theme() {
    nixe_theme::install(nixe_theme::Style {
        window_class: "glimpse-window",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "",
        extra_colors: None,
        prefs: || {
            let c = Paths::discover().load().unwrap_or_default();
            nixe_theme::ThemePrefs {
                mode: if c.mode == "theme" { nixe_theme::ThemeMode::Theme } else { nixe_theme::ThemeMode::Omarchy },
                theme: c.theme,
                glow: false,
                reduce_motion: true,
            }
        },
        set_theme: Some(|mode, theme| {
            let p = Paths::discover();
            if let Ok(mut c) = p.load() {
                c.mode = if mode == nixe_theme::ThemeMode::Theme { "theme" } else { "omarchy" }.into();
                c.theme = theme;
                let _ = p.save(&c);
            }
        }),
    });
}
fn status_error(label: &gtk::Label, e: impl std::fmt::Display) {
    label.set_text(&e.to_string());
}
thread_local! {static SEARCH:RefCell<Option<gtk::SearchEntry>>=const{RefCell::new(None)};}
pub fn present(app: &gtk::Application, toggle: bool, query: Option<&str>) {
    let _ = nixe_glimpse::ai_setup::resume(&Paths::discover());
    if let Some(win) = PANEL.with(|p| p.borrow().clone()) {
        if toggle && win.is_visible() && win.is_mapped() {
            win.set_visible(false);
            return;
        }
        // Recreate the native window instead of reusing a retired Wayland surface.
        win.destroy();
        PANEL.with(|p| *p.borrow_mut() = None);
        SEARCH.with(|s| *s.borrow_mut() = None);
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Glimpse")
        .default_width(WIDTH)
        .default_height(COLLAPSED)
        .decorated(false)
        .resizable(false)
        .build();
    window.add_css_class("glimpse-window");
    let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main.add_css_class("glimpse-frame");
    let search_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    search_bar.add_css_class("search-bar");
    let entry =
        gtk::SearchEntry::builder().placeholder_text("Search apps, files and content").search_delay(0).hexpand(true).build();
    entry.add_css_class("glimpse-search");
    search_bar.append(&entry);
    main.append(&search_bar);
    // Results on the left, the selected one previewed on the right; or an empty state.
    let content = gtk::Stack::new();
    content.set_vexpand(true);
    let results = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("results");
    let scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .width_request(LIST_WIDTH)
        .build();
    results.append(&scroll);
    let pane = PreviewPane::new(&entry);
    results.append(&pane.root);
    content.add_named(&results, Some("results"));
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_named(&empty, Some("empty"));
    content.set_visible(false);
    main.append(&content);
    let playback: Rc<RefCell<Option<Player>>> = Rc::new(RefCell::new(None));
    let media_hide = pane.media.clone();
    let playback_hide = playback.clone();
    window.connect_visible_notify(move |w| {
        if !w.is_visible() {
            stop_playback(&media_hide, &playback_hide);
            let weak = w.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(w) = weak.upgrade()
                    && !w.is_visible()
                    && let Some(app) = w.application()
                    && !app.windows().iter().any(|other| other.is_visible())
                {
                    // A dismissed launcher should release its native surface and D-Bus name.
                    app.quit();
                }
            });
        }
    });
    // Status bar: key hints for the selected result (or a message), the count, then actions.
    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    status_bar.add_css_class("status-bar");
    let hints = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    hints.add_css_class("status-hints");
    status_bar.append(&hints);
    let footer = gtk::Label::new(None);
    footer.add_css_class("status-message");
    footer.set_ellipsize(gtk::pango::EllipsizeMode::End);
    footer.set_xalign(0.);
    footer.set_visible(false);
    let hints_toggle = hints.clone();
    footer.connect_label_notify(move |label| {
        // A message replaces the hints until it is cleared.
        let message = !label.label().is_empty();
        label.set_visible(message);
        hints_toggle.set_visible(!message);
    });
    status_bar.append(&footer);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    status_bar.append(&spacer);
    let count = gtk::Label::new(None);
    count.add_css_class("status-count");
    status_bar.append(&count);
    let app_clone = app.clone();
    status_bar
        .append(&nixe_ui::chrome::status_action("folder-new-symbolic", "Add a folder to search", move || add_folder(&app_clone)));
    let app_clone = app.clone();
    status_bar
        .append(&nixe_ui::chrome::status_action("application-x-addon-symbolic", "Plugins", move || plugin_manager(&app_clone)));
    status_bar.append(&nixe_ui::chrome::gear(|| {
        dismiss();
        let _ = Command::new("nixe-settings").args(["--section", "glimpse"]).spawn();
    }));
    status_bar.set_visible(false);
    main.append(&status_bar);
    window.set_child(Some(&main));
    let armed = Rc::new(RefCell::new(None::<String>));
    let rows: Rc<RefCell<Vec<ResultRow>>> = Rc::new(RefCell::new(Vec::new()));
    let header_rows = rows.clone();
    list.set_header_func(move |row, before| {
        let results = header_rows.borrow();
        let Some(result) = results.get(row.index() as usize) else {
            return;
        };
        let same = before.and_then(|r| results.get(r.index() as usize)).is_some_and(|r| r.category().0 == result.category().0);
        if same {
            row.set_header(None::<&gtk::Widget>);
        } else {
            // GTK CSS can't uppercase, so the heading is written that way.
            let label = gtk::Label::new(Some(&result.category().1.to_uppercase()));
            label.set_xalign(0.);
            label.add_css_class("category-heading");
            if before.is_none() {
                label.add_css_class("first");
            }
            row.set_header(Some(&label));
        }
    });
    let generation = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel::<Reply>();
    let (jobs, job_rx) = mpsc::channel::<(u64, String)>();
    let apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            a.id().map(|id| {
                // Kept on this thread for the rows' icons and the app preview.
                APPS.with(|apps| apps.borrow_mut().insert(id.to_string(), a.clone()));
                let keywords = app_keywords(&id);
                (
                    id.to_string(),
                    a.display_name().to_string(),
                    format!("{} {} {keywords}", a.name(), a.description().unwrap_or_default()).to_lowercase(),
                )
            })
        })
        .collect();
    let (enrich_jobs, enrich_rx) = mpsc::channel::<(u64, String)>();
    let gen_enrich = generation.clone();
    let tx_enrich = tx.clone();
    std::thread::spawn(move || {
        let p = Paths::discover();
        let mut session: Option<semantic::Session> = None;
        let mut session_device = String::new();
        loop {
            let (mut request_generation, mut query) = match enrich_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(session) = session.as_mut() {
                        session.release_idle();
                    }
                    continue;
                }
                Err(_) => break,
            };
            std::thread::sleep(Duration::from_millis(180));
            while let Ok((g, q)) = enrich_rx.try_recv() {
                request_generation = g;
                query = q;
            }
            let q = query.trim();
            let cancelled = || gen_enrich.load(Ordering::Relaxed) != request_generation;
            if q.is_empty() || cancelled() || utility::intent(q) != utility::Intent::Search {
                continue;
            }
            let Ok(c) = p.load() else {
                continue;
            };
            if session_device != c.device || !c.semantic {
                session = None;
                session_device = c.device.clone();
            }
            let mut enrich = Vec::new();
            for (id, hit) in plugins::search_cancel(&p, q, cancelled) {
                enrich.push(ResultRow {
                    title: hit.title,
                    folder: hit.subtitle,
                    reason: String::new(),
                    action: Action::Plugin(id, hit.id, hit.action),
                });
            }
            let mut semantic_status = String::new();
            if c.semantic && query::semantic_allowed(q) && !cancelled() {
                let result = (|| {
                    if session.is_none() {
                        session = Some(semantic::Session::start(&p)?);
                    }
                    session.as_mut().unwrap().search(&p, q, cancelled)
                })();
                match result {
                    Ok(hits) => {
                        for hit in hits {
                            if let Some(path) = hit["path"].as_str() {
                                let path = PathBuf::from(path);
                                enrich.push(ResultRow {
                                    title: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                                    folder: folder_of(&path),
                                    reason: match_reason(&hit),
                                    action: Action::File(path, hit),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        semantic_status = e.to_string();
                        session = None;
                    }
                }
            }
            let _ = tx_enrich.send(Reply { generation: request_generation, rows: enrich, append: true, status: semantic_status });
        }
    });
    // `pw` queries are answered on this thread (the daemon's client runs on the GTK loop).
    let tx_pw = tx.clone();
    let gen_worker = generation.clone();
    std::thread::spawn(move || {
        // One index connection for the life of the window, not one per keystroke.
        let mut db: Option<rusqlite::Connection> = None;
        while let Ok((mut request_generation, mut query)) = job_rx.recv() {
            std::thread::sleep(Duration::from_millis(70));
            while let Ok((g, q)) = job_rx.try_recv() {
                request_generation = g;
                query = q;
            }
            if gen_worker.load(Ordering::Relaxed) != request_generation {
                continue;
            }
            let p = Paths::discover();
            let Ok(c) = p.load() else {
                continue;
            };
            let q = query.trim();
            if q.is_empty() {
                continue;
            }
            let mut found = Vec::new();
            let mut error = String::new();
            if utility::intent(q) != utility::Intent::Search {
                if utility::currency_query(q).is_some() {
                    let _ = tx.send(Reply {
                        generation: request_generation,
                        rows: vec![],
                        append: false,
                        status: "Getting daily currency rates…".into(),
                    });
                }
                match utility::answers_local(&p, q, &c, || gen_worker.load(Ordering::Relaxed) != request_generation) {
                    Ok(answers) => {
                        found.extend(answers.into_iter().map(|answer| ResultRow {
                            title: answer.title,
                            // Key hints live in the status bar, not in every row.
                            folder: answer.detail.replace(" · Enter to copy", "").replace("Enter to copy answer", ""),
                            reason: String::new(),
                            action: Action::Copy(answer.copy),
                        }));
                    }
                    Err(e) => error = e.to_string(),
                }
                let _ = tx.send(Reply { generation: request_generation, rows: found, append: false, status: error });
                continue;
            }
            let mut matching_apps: Vec<_> = apps
                .iter()
                .filter_map(|(id, name, keywords)| {
                    let rank = query::name_rank(name, q).or_else(|| query::name_rank(keywords, q).map(|r| r + 3))?;
                    Some((rank, name, id))
                })
                .collect();
            matching_apps.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
            for (_, name, id) in matching_apps.into_iter().take(12) {
                found.push(ResultRow {
                    title: name.clone(),
                    folder: String::new(),
                    reason: String::new(),
                    action: Action::App(id.clone()),
                });
            }
            if db.is_none() {
                db = index::open(&p).ok();
            }
            if let Some(conn) = &db
                && let Ok(files) = index::search_in(conn, &c, q, 30)
            {
                for f in files {
                    found.push(ResultRow {
                        title: f.name,
                        folder: folder_of(&f.path),
                        reason: String::new(),
                        action: Action::File(f.path, Value::Null),
                    });
                }
            }
            if let Some(conn) = &db
                && let Ok(hits) = index::content_search_in(conn, &p, &c, q)
            {
                for (file, info) in hits {
                    found.push(ResultRow {
                        title: file.name,
                        folder: folder_of(&file.path),
                        reason: match_reason(&info),
                        action: Action::File(file.path, info),
                    });
                }
            }
            // Matched like app names, so a stray letter doesn't offer Reboot or Shut down.
            for (name, cmd) in [("Lock screen", "lock"), ("Log out", "logout"), ("Reboot", "reboot"), ("Shut down", "shutdown")] {
                if query::name_rank(name, q).is_some() {
                    found.push(ResultRow {
                        title: name.into(),
                        folder: String::new(),
                        reason: String::new(),
                        action: Action::System(cmd),
                    });
                }
            }
            let _ = tx.send(Reply { generation: request_generation, rows: found, append: false, status: error });
            let _ = enrich_jobs.send((request_generation, query));
        }
    });
    let invalidated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = invalidated.clone();
    std::thread::spawn(move || {
        use notify::Watcher;
        let p = Paths::discover();
        let _ = p.ensure();
        let (tx, rx) = mpsc::channel();
        let Ok(mut watcher) = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event
                && matches!(
                    event.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
                )
                && event
                    .paths
                    .iter()
                    .any(|path| path.file_name().is_some_and(|name| ["settings.toml", "plugins.json"].iter().any(|s| name == *s)))
            {
                signal.store(true, Ordering::Relaxed);
            }
            let _ = tx.send(());
        }) else {
            return;
        };
        let _ = watcher.watch(&p.config, notify::RecursiveMode::NonRecursive);
        let _ = watcher.watch(&p.state, notify::RecursiveMode::NonRecursive);
        while rx.recv().is_ok() {}
    });
    let browsing = Rc::new(std::cell::Cell::new(false));
    let browsing_poll = browsing.clone();
    let rebuilding = Rc::new(std::cell::Cell::new(false));
    let rebuild_poll = rebuilding.clone();
    let scroll_poll = scroll.clone();
    let refresh_jobs = jobs.clone();
    let refresh_entry = entry.clone();
    let rows_poll = rows.clone();
    let list_poll = list.clone();
    let footer_poll = footer.clone();
    let count_poll = count.clone();
    let content_poll = content.clone();
    let empty_poll = empty.clone();
    let gen_poll = generation.clone();
    let pane_poll = pane.root.clone();
    let window_weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(25), move || {
        if window_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        if invalidated.swap(false, Ordering::Relaxed) {
            pane_poll.set_visible(Paths::discover().load().is_ok_and(|c| c.preview));
            // A `pw` query never goes to the other providers.
            if passwords::parse(&refresh_entry.text()).is_none() {
                let g = gen_poll.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = refresh_jobs.send((g, refresh_entry.text().to_string()));
            }
        }
        while let Ok(reply) = rx.try_recv() {
            if reply.generation != gen_poll.load(Ordering::Relaxed) {
                continue;
            }
            // Late enrichment must never move the list under keyboard or mouse navigation:
            // new results are added at the end, leaving the selection and scroll position alone.
            if browsing_poll.get() {
                if !reply.status.is_empty() {
                    footer_poll.set_text(&reply.status);
                }
                for result in reply.rows {
                    if rows_poll.borrow().iter().any(|r| r.identity() == result.identity()) {
                        continue;
                    }
                    rows_poll.borrow_mut().push(result.clone());
                    list_poll.append(&result_row(&result));
                }
                count_poll.set_text(&count_text(rows_poll.borrow().len()));
                continue;
            }
            if !reply.status.is_empty() {
                footer_poll.set_text(&reply.status);
            }
            let mut merged = if reply.append { rows_poll.borrow().clone() } else { Vec::new() };
            for result in reply.rows {
                if let Some(existing) = merged.iter_mut().find(|r| r.identity() == result.identity()) {
                    if matches!(&result.action, Action::File(_, hit) if !hit.is_null()) {
                        let mut result = result;
                        // A file already listed by name keeps its place rather than moving to Related.
                        if existing.category().0 != RELATED
                            && let Action::File(_, ref mut hit) = result.action
                        {
                            hit["related"] = false.into();
                        }
                        result.reason = match &result.action {
                            Action::File(_, hit) => match_reason(hit),
                            _ => result.reason,
                        };
                        *existing = result;
                    }
                } else {
                    merged.push(result);
                }
            }
            merged.sort_by_key(|r| r.category().0);
            if merged.is_empty() && reply.status.is_empty() {
                show_empty(&empty_poll, refresh_entry.text().trim());
                content_poll.set_visible_child_name("empty");
            } else {
                content_poll.set_visible_child_name("results");
            }
            count_poll.set_text(&count_text(merged.len()));
            let unchanged = {
                let old = rows_poll.borrow();
                old.len() == merged.len()
                    && old.iter().zip(&merged).all(|(a, b)| {
                        a.identity() == b.identity() && a.title == b.title && a.folder == b.folder && a.reason == b.reason
                    })
            };
            if unchanged {
                *rows_poll.borrow_mut() = merged;
                continue;
            }
            rebuild_poll.set(true);
            while let Some(child) = list_poll.first_child() {
                list_poll.remove(&child);
            }
            *rows_poll.borrow_mut() = merged.clone();
            for result in &merged {
                list_poll.append(&result_row(result));
            }
            // Nobody has navigated yet (that path returns above), so the top result is the
            // one selected; otherwise Enter could act on a row sorted out of view.
            list_poll.select_row(list_poll.row_at_index(0).as_ref());
            rebuild_poll.set(false);
            // Notify once, after the list is complete.
            list_poll.emit_by_name::<()>("row-selected", &[&list_poll.selected_row()]);
            let adjustment = scroll_poll.vadjustment();
            glib::idle_add_local_once(move || adjustment.set_value(0.));
        }
        glib::ControlFlow::Continue
    });
    let jobs_entry = jobs.clone();
    let gen_entry = generation.clone();
    let footer_entry = footer.clone();
    let count_entry = count.clone();
    let hints_entry = hints.clone();
    let armed_entry = armed.clone();
    let content_entry = content.clone();
    let status_entry = status_bar.clone();
    let pane_entry = pane.clone();
    let playback_entry = playback.clone();
    let window_entry = window.clone();
    let list_entry = list.clone();
    let rows_entry = rows.clone();
    let browsing_entry = browsing.clone();
    entry.connect_search_changed(move |e| {
        browsing_entry.set(false);
        let expanded = !e.text().trim().is_empty();
        let is_math = utility::intent(&e.text()) == utility::Intent::Calculation && passwords::parse(&e.text()).is_none();
        pane_entry.show_calculator(is_math);
        stop_playback(&pane_entry.media, &playback_entry);
        if is_math {
            let c = Paths::discover().load().unwrap_or_default();
            pane_entry.display.set_text(
                &utility::answers(&e.text(), &c)
                    .ok()
                    .and_then(|a| a.into_iter().next())
                    .map(|a| a.title)
                    .unwrap_or_else(|| utility::pretty_math(&e.text())),
            );
        }
        content_entry.set_visible(expanded);
        content_entry.set_visible_child_name("results");
        // The bar's hairline only shows once there is something under it.
        if let Some(frame) = content_entry.parent() {
            if expanded { frame.add_css_class("expanded") } else { frame.remove_css_class("expanded") }
        }
        status_entry.set_visible(expanded);
        pane_entry.root.set_visible(Paths::discover().load().is_ok_and(|c| c.preview));
        window_entry.set_default_size(WIDTH, if expanded { EXPANDED } else { COLLAPSED });
        while let Some(child) = list_entry.first_child() {
            list_entry.remove(&child);
        }
        rows_entry.borrow_mut().clear();
        *armed_entry.borrow_mut() = None;
        footer_entry.set_text("");
        count_entry.set_text("");
        set_hints(&hints_entry, None);
        let g = gen_entry.fetch_add(1, Ordering::Relaxed) + 1;
        // `pw …` belongs to the vault alone: no apps, files, plugins or semantic search.
        if let Some(q) = passwords::parse(&e.text()) {
            let tx = tx_pw.clone();
            glib::spawn_future_local(async move {
                let (found, status) = pw::lookup(&q).await;
                let rows = found
                    .into_iter()
                    .map(|(title, folder, p)| ResultRow { title, folder, reason: String::new(), action: Action::Password(p) })
                    .collect();
                let _ = tx.send(Reply { generation: g, rows, append: false, status });
            });
            return;
        }
        let _ = jobs_entry.send((g, e.text().to_string()));
    });
    let preview_gen = Arc::new(AtomicU64::new(0));
    let (preview_tx, preview_rx) = mpsc::channel::<(u64, Preview)>();
    let rows_preview = rows.clone();
    let gen_preview = preview_gen.clone();
    let query_entry = entry.clone();
    let armed_preview = armed.clone();
    let footer_preview = footer.clone();
    let hints_preview = hints.clone();
    let selected_preview = Rc::new(RefCell::new(None::<String>));
    let pane_select = pane.clone();
    let playback_select = playback.clone();
    let window_select = window.clone();
    let entry_select = entry.clone();
    list.connect_row_selected(move |_, row| {
        if rebuilding.get() {
            return;
        }
        // Moving away from a system action disarms it.
        if armed_preview.borrow_mut().take().is_some() {
            footer_preview.set_text("");
        }
        if row.is_some() {
            let window = window_select.clone();
            let entry = entry_select.clone();
            glib::idle_add_local_once(move || {
                if window.is_visible() {
                    focus_search(&window, &entry);
                }
            });
        }
        let pane = &pane_select;
        let Some(row) = row else {
            *selected_preview.borrow_mut() = None;
            gen_preview.fetch_add(1, Ordering::Relaxed);
            stop_playback(&pane.media, &playback_select);
            pane.card(&gio::ThemedIcon::new("system-search-symbolic").upcast(), "", "", true);
            pane.clear_details();
            set_hints(&hints_preview, None);
            return;
        };
        let Some(result) = rows_preview.borrow().get(row.index() as usize).cloned() else {
            return;
        };
        let identity = result.identity();
        if selected_preview.borrow().as_ref() == Some(&identity) {
            return;
        }
        *selected_preview.borrow_mut() = Some(identity);
        set_hints(&hints_preview, Some(&result.action));
        let g = gen_preview.fetch_add(1, Ordering::Relaxed) + 1;
        stop_playback(&pane.media, &playback_select);
        pane.clear_details();
        let icon = action_icon(&result.action);
        match &result.action {
            Action::App(id) => {
                let app = APPS.with(|apps| apps.borrow().get(id).cloned());
                let description = app.as_ref().and_then(|a| a.description()).map(|d| d.to_string()).unwrap_or_default();
                pane.card(&icon, "", &description, false);
                pane.add_detail("Kind", "Application");
                if let Some(command) = app.and_then(|a| a.commandline()) {
                    // Drop the desktop-entry field codes (%U, %f, …) from the command shown.
                    let command = command
                        .to_string_lossy()
                        .split_whitespace()
                        .filter(|w| !w.starts_with('%'))
                        .collect::<Vec<_>>()
                        .join(" ");
                    pane.add_detail("Command", &command);
                }
                return;
            }
            Action::Copy(value) => {
                pane.card(&icon, &result.title, &result.folder, false);
                pane.add_detail("Copies", value);
                return;
            }
            Action::System(cmd) => {
                pane.card(&icon, "", system_description(cmd), false);
                return;
            }
            Action::Password(p) => {
                pane.card(&icon, &result.title, &result.folder, false);
                for (key, value) in p.details() {
                    pane.add_detail(&key, &value);
                }
                return;
            }
            Action::File(path, _) => {
                // Until the preview arrives, a dim icon holds the frame so it doesn't flash.
                pane.card(&icon, "", "", true);
                let c = Paths::discover().load().unwrap_or_default();
                if c.preview
                    && config::allowed(&c, path)
                    && let Some(is_video) = media_kind(path)
                    && let Some(player) = Player::new(path, is_video)
                {
                    if !is_video {
                        let art = gtk::Image::from_gicon(&icon);
                        art.set_pixel_size(64);
                        art.set_vexpand(true);
                        art.add_css_class("preview-card-icon");
                        pane.media.append(&art);
                    }
                    pane.media.append(player.widget());
                    pane.frame.set_visible_child_name("media");
                    *playback_select.borrow_mut() = Some(player);
                }
            }
            Action::Plugin(..) => pane.card(&icon, "", "", true),
        }
        let query = query_entry.text().to_string();
        if utility::intent(&query) == utility::Intent::Calculation {
            return;
        }
        let tx = preview_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((g, make_preview(&result, &query)));
        });
    });
    let pane_poll = pane.clone();
    let win_weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(35), move || {
        if win_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        while let Ok((g, preview)) = preview_rx.try_recv() {
            if g != preview_gen.load(Ordering::Relaxed) {
                continue;
            }
            pane_poll.set_details(&preview.details);
            match preview.body {
                Body::Icon => pane_poll.card_icon.remove_css_class("dim"),
                Body::Text(markup) => pane_poll.text(&markup),
                Body::Image(bytes) => match gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) {
                    Ok(texture) => pane_poll.image(&texture),
                    Err(_) => pane_poll.card_icon.remove_css_class("dim"),
                },
                Body::Media => {}
            }
        }
        glib::ControlFlow::Continue
    });
    let rows_activate = rows.clone();
    let win_activate = window.clone();
    let footer_activate = footer.clone();
    let armed_activate = armed.clone();
    list.connect_row_activated(move |_, row| {
        if let Some(result) = rows_activate.borrow().get(row.index() as usize) {
            if let Action::Password(p) = &result.action {
                pw::activate(p, None, &win_activate, &footer_activate);
                return;
            }
            activate(result, &win_activate, &footer_activate, &armed_activate, false);
        }
    });
    let keys = gtk::EventControllerKey::new();
    let list_keys = list.clone();
    let win_keys = window.clone();
    let rows_keys = rows.clone();
    let footer_keys = footer.clone();
    let entry_keys = entry.clone();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let scroll_keys = scroll.clone();
    let browsing_keys = browsing.clone();
    let playback_keys = playback.clone();
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |_, _, _, _| browsing.set(true));
    list.add_controller(click);
    keys.connect_key_pressed(move |_, key, _, mods| {
        match key {
            gdk::Key::Escape => {
                win_keys.set_visible(false);
            }
            gdk::Key::Down | gdk::Key::Up | gdk::Key::Page_Down | gdk::Key::Page_Up => {
                let last = rows_keys.borrow().len() as i32 - 1;
                let step = match key {
                    gdk::Key::Down => 1,
                    gdk::Key::Up => -1,
                    gdk::Key::Page_Down => 8,
                    _ => -8,
                };
                let next = list_keys.selected_row().map(|r| (r.index() + step).clamp(0, last.max(0))).unwrap_or(0);
                select_and_reveal(&list_keys, &scroll_keys, &browsing_keys, next);
            }
            // Plain Home/End stay with the search text; with Ctrl they jump through the results.
            gdk::Key::Home | gdk::Key::End if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                let last = rows_keys.borrow().len() as i32 - 1;
                select_and_reveal(&list_keys, &scroll_keys, &browsing_keys, if key == gdk::Key::Home { 0 } else { last.max(0) });
            }
            gdk::Key::p if mods.contains(gdk::ModifierType::CONTROL_MASK) => match playback_keys.borrow().as_ref() {
                Some(player) => player.toggle(),
                None => return glib::Propagation::Proceed,
            },
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if let Some(row) = list_keys.selected_row()
                    && let Some(result) = rows_keys.borrow().get(row.index() as usize)
                {
                    if let Action::Password(p) = &result.action {
                        // Ctrl+Enter copies the password, Alt+Enter the one-time code.
                        let verb = if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                            Some(passwords::Verb::Password)
                        } else if mods.contains(gdk::ModifierType::ALT_MASK) {
                            Some(passwords::Verb::Totp)
                        } else {
                            None
                        };
                        pw::activate(p, verb, &win_keys, &footer_keys);
                        return glib::Propagation::Stop;
                    }
                    activate(result, &win_keys, &footer_keys, &armed, mods.contains(gdk::ModifierType::CONTROL_MASK));
                }
            }
            gdk::Key::o | gdk::Key::O if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                let selected = list_keys.selected_row().and_then(|row| rows_keys.borrow().get(row.index() as usize).cloned());
                match selected.map(|r| r.action) {
                    Some(Action::Password(p)) => pw::activate(&p, Some(passwords::Verb::Open), &win_keys, &footer_keys),
                    _ => return glib::Propagation::Proceed,
                }
            }
            // With text selected in the search box Ctrl+C copies that; otherwise the selected result.
            gdk::Key::c if mods.contains(gdk::ModifierType::CONTROL_MASK) && entry_keys.selection_bounds().is_none() => {
                let Some(row) = list_keys.selected_row() else { return glib::Propagation::Proceed };
                let Some(result) = rows_keys.borrow().get(row.index() as usize).cloned() else {
                    return glib::Propagation::Proceed;
                };
                // A vault row copies its username through the daemon, never its title.
                if let Action::Password(p) = &result.action {
                    pw::activate(p, Some(passwords::Verb::Username), &win_keys, &footer_keys);
                    return glib::Propagation::Stop;
                }
                let text = match &result.action {
                    Action::File(path, _) => path.display().to_string(),
                    Action::Copy(value) => value.clone(),
                    _ => result.title.clone(),
                };
                win_keys.clipboard().set_text(&text);
                footer_keys.set_text("Copied");
                let footer = footer_keys.clone();
                glib::timeout_add_local_once(Duration::from_secs(2), move || {
                    if footer.text() == "Copied" {
                        footer.set_text("");
                    }
                });
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
    let next_list = list.clone();
    entry.connect_next_match(move |_| navigate_result(&next_list, 1));
    let previous_list = list.clone();
    entry.connect_previous_match(move |_| navigate_result(&previous_list, -1));
    window.connect_close_request(|w| {
        w.set_visible(false);
        glib::Propagation::Stop
    });
    // A click elsewhere, or another window taking the keyboard, puts the launcher away.
    window.connect_is_active_notify(|w| {
        if !w.is_active() && w.is_visible() {
            w.set_visible(false);
        }
    });
    PANEL.with(|p| *p.borrow_mut() = Some(window.clone()));
    window.present();
    focus_search(&window, &entry);
    SEARCH.with(|s| *s.borrow_mut() = Some(entry.clone()));
    if let Some(query) = query {
        entry.set_text(query);
    }
}
/// The right-hand pane: a framed content area, then a list of details. Every kind of
/// result uses the same layout, so moving through results never reshuffles the pane.
#[derive(Clone)]
struct PreviewPane {
    root: gtk::Box,
    frame: gtk::Stack,
    text: gtk::Label,
    text_scroll: gtk::ScrolledWindow,
    picture: gtk::Picture,
    media: gtk::Box,
    card_icon: gtk::Image,
    card_title: gtk::Label,
    card_desc: gtk::Label,
    details: gtk::Grid,
    detail_rows: Rc<std::cell::Cell<i32>>,
    calculator: gtk::Box,
    display: gtk::Label,
}
impl PreviewPane {
    fn new(entry: &gtk::SearchEntry) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.add_css_class("preview");
        root.set_hexpand(true);
        let frame = gtk::Stack::new();
        frame.add_css_class("preview-frame");
        frame.set_vexpand(true);
        frame.set_overflow(gtk::Overflow::Hidden);
        let text = gtk::Label::new(None);
        text.set_wrap(true);
        text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        text.set_xalign(0.);
        text.set_yalign(0.);
        text.add_css_class("preview-text");
        let text_scroll = gtk::ScrolledWindow::builder()
            .vscrollbar_policy(gtk::PolicyType::External)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&text)
            .build();
        frame.add_named(&text_scroll, Some("text"));
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.add_css_class("preview-picture");
        frame.add_named(&picture, Some("image"));
        let media = gtk::Box::new(gtk::Orientation::Vertical, 0);
        media.add_css_class("preview-media");
        frame.add_named(&media, Some("media"));
        let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
        card.add_css_class("preview-card");
        card.set_valign(gtk::Align::Center);
        card.set_halign(gtk::Align::Center);
        let card_icon = gtk::Image::new();
        card_icon.set_pixel_size(64);
        card.append(&card_icon);
        let card_title = gtk::Label::new(None);
        card_title.set_wrap(true);
        card_title.set_justify(gtk::Justification::Center);
        card_title.set_selectable(false);
        card_title.add_css_class("preview-card-title");
        card.append(&card_title);
        let card_desc = gtk::Label::new(None);
        card_desc.set_wrap(true);
        card_desc.set_justify(gtk::Justification::Center);
        card_desc.set_max_width_chars(42);
        card_desc.add_css_class("preview-card-desc");
        card.append(&card_desc);
        frame.add_named(&card, Some("card"));
        let details = gtk::Grid::builder().row_spacing(6).column_spacing(18).build();
        details.add_css_class("preview-details");
        let (calculator, display) = calculator_preview(entry);
        calculator.set_visible(false);
        calculator.set_vexpand(true);
        root.append(&frame);
        root.append(&details);
        root.append(&calculator);
        Self {
            root,
            frame,
            text,
            text_scroll,
            picture,
            media,
            card_icon,
            card_title,
            card_desc,
            details,
            detail_rows: Rc::new(std::cell::Cell::new(0)),
            calculator,
            display,
        }
    }
    fn show_calculator(&self, on: bool) {
        self.frame.set_visible(!on);
        self.details.set_visible(!on);
        self.calculator.set_visible(on);
    }
    /// A large icon with an optional title and line of text, for results with nothing
    /// else to show. A dim icon is the placeholder while a preview loads.
    fn card(&self, icon: &gio::Icon, title: &str, desc: &str, dim: bool) {
        self.card_icon.set_from_gicon(icon);
        self.card_icon.set_css_classes(if dim { &["preview-card-icon", "dim"][..] } else { &["preview-card-icon"][..] });
        self.card_title.set_text(title);
        self.card_title.set_visible(!title.is_empty());
        self.card_desc.set_text(desc);
        self.card_desc.set_visible(!desc.is_empty());
        self.frame.set_visible_child_name("card");
    }
    fn text(&self, markup: &str) {
        self.text.set_markup(markup);
        self.text_scroll.vadjustment().set_value(0.);
        self.frame.set_visible_child_name("text");
    }
    fn image(&self, texture: &gdk::Texture) {
        self.picture.set_paintable(Some(texture));
        self.frame.set_visible_child_name("image");
    }
    fn clear_details(&self) {
        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        self.detail_rows.set(0);
    }
    fn add_detail(&self, key: &str, value: &str) {
        let row = self.detail_rows.get();
        let k = gtk::Label::new(Some(key));
        k.set_xalign(0.);
        k.set_valign(gtk::Align::Start);
        k.add_css_class("detail-key");
        let v = gtk::Label::new(Some(value));
        v.set_xalign(0.);
        v.set_hexpand(true);
        // A folder keeps its end; anything else keeps its start.
        v.set_ellipsize(if key == "Folder" { gtk::pango::EllipsizeMode::Start } else { gtk::pango::EllipsizeMode::End });
        v.set_tooltip_text(Some(value));
        v.add_css_class("detail-value");
        self.details.attach(&k, 0, row, 1, 1);
        self.details.attach(&v, 1, row, 1, 1);
        self.detail_rows.set(row + 1);
    }
    fn set_details(&self, details: &[(String, String)]) {
        self.clear_details();
        for (key, value) in details {
            self.add_detail(key, value);
        }
    }
}
/// What the preview thread hands back: the content to frame, and the details below it.
enum Body {
    /// Nothing to show beyond the details: the frame shows the result's icon.
    Icon,
    /// Pango markup for the text frame.
    Text(String),
    /// An encoded image (a picture, a PDF page, a video still).
    Image(Vec<u8>),
    /// Leave the inline player that was attached on selection.
    Media,
}
struct Preview {
    body: Body,
    details: Vec<(String, String)>,
}
thread_local! {static APPS: RefCell<std::collections::HashMap<String, gio::AppInfo>> = RefCell::new(Default::default());}
fn action_icon(action: &Action) -> gio::Icon {
    let icon = match action {
        Action::App(id) => APPS.with(|apps| apps.borrow().get(id).and_then(|app| app.icon())),
        Action::File(path, _) => Some(gio::content_type_get_symbolic_icon(&content_type(path))),
        _ => None,
    };
    icon.unwrap_or_else(|| gio::ThemedIcon::new(symbolic_icon(action)).upcast())
}
const RELATED: u8 = 5;
fn count_text(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => "1 result".into(),
        n => format!("{n} results"),
    }
}
fn system_description(cmd: &str) -> &'static str {
    match cmd {
        "lock" => "Lock the screen and turn off the display.",
        "logout" => "Log out after closing application windows.",
        "reboot" => "Restart after closing application windows.",
        _ => "Shut down after closing application windows.",
    }
}
fn show_empty(slot: &gtk::Box, query: &str) {
    while let Some(child) = slot.first_child() {
        slot.remove(&child);
    }
    slot.append(&nixe_ui::widgets::empty_state(
        "system-search-symbolic",
        &format!("No results for “{query}”"),
        "Try fewer or different words, or add a folder to search.",
        None,
    ));
}
/// The status bar's key hints for what Enter and the shortcuts do to this result.
fn set_hints(hints: &gtk::Box, action: Option<&Action>) {
    while let Some(child) = hints.first_child() {
        hints.remove(&child);
    }
    let enter: &[&str] = &["Enter"];
    let list: Vec<(&[&str], &str)> = match action {
        Some(Action::File(path, _)) => {
            let mut list = vec![(enter, "Open"), (&["Ctrl", "Enter"][..], "Reveal"), (&["Ctrl", "C"][..], "Copy")];
            if media_kind(path).is_some() {
                list.push((&["Ctrl", "P"][..], "Play"));
            }
            list
        }
        Some(Action::App(_)) => vec![(enter, "Launch")],
        Some(Action::Copy(_)) => vec![(enter, "Copy")],
        Some(Action::System("lock")) => vec![(enter, "Lock")],
        Some(Action::System(_)) => vec![(enter, "Run")],
        Some(Action::Plugin(..)) => vec![(enter, "Open")],
        Some(Action::Password(p)) => p.hints(),
        None => vec![],
    };
    for (keys, label) in list {
        let hint = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        hint.append(&nixe_ui::widgets::key_caps(keys));
        hint.append(&gtk::Label::new(Some(label)));
        hints.append(&hint);
    }
}
fn symbolic_icon(action: &Action) -> &'static str {
    match action {
        Action::App(_) => "application-x-executable-symbolic",
        Action::File(..) => "text-x-generic-symbolic",
        Action::Copy(_) => "accessories-calculator-symbolic",
        Action::System("lock") => "system-lock-screen-symbolic",
        Action::System("logout") => "system-log-out-symbolic",
        Action::System("reboot") => "system-reboot-symbolic",
        Action::System(_) => "system-shutdown-symbolic",
        Action::Plugin(..) => "application-x-addon-symbolic",
        Action::Password(p) => p.icon(),
    }
}
/// Apps show their own icon; files the symbolic icon for their type.
fn result_icon(action: &Action) -> gtk::Image {
    gtk::Image::from_gicon(&action_icon(action))
}
fn result_row(result: &ResultRow) -> gtk::ListBoxRow {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    line.add_css_class("result");
    let icon = result_icon(&result.action);
    icon.set_pixel_size(20);
    icon.set_valign(gtk::Align::Center);
    icon.add_css_class(if matches!(result.action, Action::App(_)) { "result-app-icon" } else { "result-icon" });
    line.append(&icon);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let title = gtk::Label::new(Some(&result.title));
    title.set_xalign(0.);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.add_css_class("result-title");
    text.append(&title);
    if !result.folder.is_empty() {
        let folder = gtk::Label::new(Some(&result.folder));
        folder.set_xalign(0.);
        // A folder keeps its end, which is what tells two README.md files apart.
        let file = matches!(result.action, Action::File(..));
        folder.set_ellipsize(if file { gtk::pango::EllipsizeMode::Start } else { gtk::pango::EllipsizeMode::End });
        folder.add_css_class("result-folder");
        text.append(&folder);
    }
    line.append(&text);
    if !result.reason.is_empty() {
        line.append(&nixe_ui::widgets::tag(&result.reason, if result.reason == "Related" { "" } else { "accent" }));
    }
    let row = gtk::ListBoxRow::new();
    row.add_css_class(if result.folder.is_empty() { "single" } else { "double" });
    if let Action::File(path, _) = &result.action {
        row.set_tooltip_text(Some(&path.display().to_string()));
    }
    row.set_child(Some(&line));
    row
}
fn calculator_preview(entry: &gtk::SearchEntry) -> (gtk::Box, gtk::Label) {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("calculator");
    let display = gtk::Label::new(None);
    display.set_wrap(true);
    display.set_selectable(true);
    display.set_xalign(1.);
    display.add_css_class("calculation-display");
    root.append(&display);
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(6).column_homogeneous(true).build();
    for (i, label) in ["C", "⌫", "(", ")", "7", "8", "9", "÷", "4", "5", "6", "×", "1", "2", "3", "−", "0", ".", "=", "+"]
        .into_iter()
        .enumerate()
    {
        let button = gtk::Button::with_label(label);
        button.set_hexpand(true);
        button.set_focusable(false);
        let entry = entry.clone();
        button.connect_clicked(move |_| {
            match label {
                "C" => {
                    entry.set_text("=");
                    entry.set_position(-1);
                }
                "⌫" => {
                    if let Some((start, end)) = entry.selection_bounds() {
                        entry.delete_text(start, end);
                        entry.set_position(start);
                    } else {
                        let end = entry.position();
                        if end > 0 {
                            entry.delete_text(end - 1, end);
                            entry.set_position(end - 1);
                        }
                    }
                }
                "=" => {}
                text => {
                    let mut pos = entry.position();
                    if let Some((start, end)) = entry.selection_bounds() {
                        entry.delete_text(start, end);
                        pos = start;
                    }
                    entry.insert_text(text, &mut pos);
                    entry.set_position(pos);
                }
            }
            let position = entry.position();
            entry.grab_focus();
            entry.select_region(position, position);
            entry.set_position(position);
        });
        grid.attach(&button, (i % 4) as i32, (i / 4) as i32, 1, 1);
    }
    root.append(&grid);
    let copy = gtk::Button::with_label("Copy answer");
    copy.set_focusable(false);
    let input = entry.clone();
    copy.connect_clicked(move |_| {
        if let Ok(answers) = utility::answers(&input.text(), &Paths::discover().load().unwrap_or_default())
            && let Some(answer) = answers.first()
        {
            input.clipboard().set_text(&answer.copy);
        }
        let position = input.position();
        input.grab_focus();
        input.select_region(position, position);
        input.set_position(position);
    });
    root.append(&copy);
    (root, display)
}
/// Builds a file or plugin preview off the main thread. Apps, utilities and system
/// actions are previewed directly on selection.
fn make_preview(result: &ResultRow, query: &str) -> Preview {
    let note = |text: &str| Preview { body: Body::Text(glib::markup_escape_text(text).to_string()), details: vec![] };
    let p = Paths::discover();
    let Ok(c) = p.load() else {
        return note("Glimpse's preferences could not be read.");
    };
    match &result.action {
        Action::File(path, hit) => {
            if !config::allowed(&c, path) {
                return note("This folder is no longer searched.");
            }
            let Ok(meta) = path.metadata() else {
                return note("This file is no longer available.");
            };
            let mut details = file_details(path, &meta, hit);
            let body = file_body(path, hit, query, &mut details);
            Preview { body, details }
        }
        Action::Plugin(id, result_id, _) => {
            let plugin = plugins::get(&p, id).ok();
            let mut details = vec![("Plugin".to_string(), plugin.as_ref().map_or(id.clone(), |p| p.manifest.name.clone()))];
            if !result.folder.is_empty() {
                details.insert(0, ("Result".into(), result.folder.clone()));
            }
            let text = plugin
                .filter(|plugin| plugin.manifest.capabilities.contains(&"preview".into()))
                .and_then(|_| {
                    plugins::call(&p, id, "preview", "preview", json!({"result_id":result_id,"title":result.title,"query":query}))
                        .ok()
                })
                .and_then(|v| v["text"].as_str().map(|s| s.chars().take(16000).collect::<String>()));
            Preview { body: text.map_or(Body::Icon, |t| Body::Text(highlight(&t, query))), details }
        }
        _ => Preview { body: Body::Icon, details: vec![] },
    }
}
/// Kind, size, date, folder and where the match is: never the name, which the list shows.
fn file_details(path: &Path, meta: &std::fs::Metadata, hit: &Value) -> Vec<(String, String)> {
    let kind = gio::content_type_get_description(&content_type(path)).to_string();
    let mut chars = kind.chars();
    let kind = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or(kind);
    let mut details = vec![("Kind".to_string(), kind)];
    if meta.is_file() {
        details.push(("Size".into(), glib::format_size(meta.len()).to_string()));
    }
    if let Ok(modified) = meta.modified() {
        details.push(("Modified".into(), when(modified)));
    }
    details.push(("Folder".into(), folder_of(path)));
    if let Some(place) = match_place(hit) {
        details.push(("Match".into(), place));
    } else if hit["related"] == true {
        details.push(("Match".into(), "Similar meaning".into()));
    } else if !hit.is_null() {
        details.push(("Match".into(), "In the contents".into()));
    }
    details
}
/// "Today, 14:02", "Yesterday, 09:15", "Oct 12" or "Oct 12, 2024".
fn when(time: std::time::SystemTime) -> String {
    let secs = time.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (Ok(at), Ok(now)) = (glib::DateTime::from_unix_local(secs), glib::DateTime::now_local()) else {
        return String::new();
    };
    let yesterday = now.add_days(-1).map(|d| d.ymd()).ok();
    let format = if at.ymd() == now.ymd() {
        "Today, %H:%M"
    } else if Some(at.ymd()) == yesterday {
        "Yesterday, %H:%M"
    } else if at.year() == now.year() {
        "%b %-d"
    } else {
        "%b %-d, %Y"
    };
    at.format(format).map(|s| s.to_string()).unwrap_or_default()
}
fn capture_image(cmd: Command) -> Option<Vec<u8>> {
    nixe_glimpse::process::capture(cmd, vec![], Duration::from_secs(3), 6 * 1024 * 1024).ok().filter(|b| !b.is_empty())
}
fn file_body(path: &Path, hit: &Value, query: &str, details: &mut Vec<(String, String)>) -> Body {
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
    if let Some(is_video) = media_kind(path) {
        if let Some(seconds) = media_length(path) {
            details.insert(2, ("Length".into(), clock(seconds)));
        }
        if crate::player::available() {
            return Body::Media;
        }
        // Without libmpv: a still from a few seconds in, past any black intro.
        if is_video {
            for seek in ["5", "0"] {
                let mut cmd = Command::new("ffmpeg");
                cmd.args(["-v", "error", "-threads", "1", "-ss", seek, "-i"]).arg(path).args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=800:800:force_original_aspect_ratio=decrease",
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "png",
                    "-",
                ]);
                if let Some(bytes) = capture_image(cmd) {
                    return Body::Image(bytes);
                }
            }
        }
        return Body::Icon;
    }
    if ext == "pdf" {
        let page = hit["info"]["page"]
            .as_u64()
            .or_else(|| hit["loc"].as_f64().filter(|_| hit["kind"] == "pdf").map(|n| n as u64))
            .unwrap_or(1)
            .max(1)
            .to_string();
        let mut image = Command::new("pdftoppm");
        image.args(["-f", &page, "-l", &page, "-singlefile", "-scale-to", "800", "-png"]).arg(path);
        if let Some(bytes) = capture_image(image) {
            return Body::Image(bytes);
        }
        let mut cmd = Command::new("pdftotext");
        cmd.args(["-f", &page, "-l", &page, "--"]).arg(path).arg("-");
        if let Ok(bytes) = nixe_glimpse::process::capture(cmd, vec![], Duration::from_secs(3), 65536) {
            return text_body(&String::from_utf8_lossy(&bytes), path, hit, query);
        }
        return Body::Icon;
    }
    if ["png", "jpg", "jpeg", "webp", "gif", "heic", "heif", "bmp", "tiff", "tif", "svg", "avif"].contains(&ext.as_str()) {
        if let Some((_, width, height)) = gtk::gdk_pixbuf::Pixbuf::file_info(path) {
            details.insert(1, ("Dimensions".into(), format!("{width} × {height}")));
        }
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-threads", "1", "-i"]).arg(path).args([
            "-frames:v",
            "1",
            "-vf",
            "scale=800:800:force_original_aspect_ratio=decrease",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ]);
        return capture_image(cmd).map_or(Body::Icon, Body::Image);
    }
    if ["docx", "odt", "pptx", "xlsx", "ods", "odp"].contains(&ext.as_str()) {
        let member = match ext.as_str() {
            "docx" => "word/document.xml",
            "pptx" => "ppt/slides/slide*.xml",
            "xlsx" => "xl/sharedStrings.xml",
            _ => "content.xml",
        };
        let mut cmd = Command::new("unzip");
        cmd.arg("-p").arg(path).arg(member);
        if let Ok(bytes) = nixe_glimpse::process::capture(cmd, vec![], Duration::from_secs(3), 2 * 1024 * 1024) {
            let text = semantic::strip_markup(&String::from_utf8_lossy(&bytes));
            // Office XML has no line breaks worth keeping; collapse the runs of spaces.
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            return text_body(&text, path, hit, query);
        }
        return Body::Icon;
    }
    if let Ok(f) = std::fs::File::open(path) {
        let mut bytes = Vec::new();
        if f.take(65536).read_to_end(&mut bytes).is_ok() && !bytes.is_empty() && !bytes.contains(&0) {
            return text_body(&String::from_utf8_lossy(&bytes), path, hit, query);
        }
    }
    Body::Icon
}
/// The passage that matched: the hit's line, the semantic excerpt, or the first line
/// with the search word; the start of the file when nothing points elsewhere.
fn text_body(text: &str, path: &Path, hit: &Value, query: &str) -> Body {
    let keyword = query.split_whitespace().find(|s| s.chars().count() >= 3).unwrap_or("").to_lowercase();
    let line_hit = hit["info"]["line"]
        .as_u64()
        .or_else(|| hit["loc"].as_f64().filter(|_| hit["segment"] == "lines").map(|n| n as u64))
        .map(|n| n as usize);
    let keyword_line = (!keyword.is_empty())
        .then(|| text.to_lowercase().find(&keyword).map(|at| text.to_lowercase()[..at].matches('\n').count() + 1))
        .flatten();
    let from_line = |line: usize| text.lines().skip(line.saturating_sub(3)).collect::<Vec<_>>().join("\n");
    let excerpt = if let Some(line) = line_hit {
        from_line(line)
    } else if let Some(line) = keyword_line {
        from_line(line)
    } else if let Some(excerpt) = hit["excerpt"].as_str() {
        excerpt.to_string()
    } else {
        text.to_string()
    };
    let excerpt: String = excerpt.trim_start_matches('\n').chars().take(12000).collect();
    Body::Text(code_markup(&excerpt, path).unwrap_or_else(|| highlight(&excerpt, query)))
}
fn media_length(path: &Path) -> Option<f64> {
    let mut probe = Command::new("ffprobe");
    probe.args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"]).arg(path);
    let bytes = nixe_glimpse::process::capture(probe, vec![], Duration::from_secs(2), 256).ok()?;
    String::from_utf8_lossy(&bytes).trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.)
}
fn activate(
    result: &ResultRow,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    armed: &RefCell<Option<String>>,
    reveal: bool,
) {
    if let Action::Plugin(id, result_id, plugins::Action::Activate { token }) = &result.action {
        if let Some(flag) = plugins::capture_host_args(id, token) {
            match plugins::spawn_capture(flag) {
                Ok(()) => window.set_visible(false),
                Err(e) => status_error(status, e),
            }
            return;
        }
        let (id, result_id, token) = (id.clone(), result_id.clone(), token.clone());
        let (tx, rx) = mpsc::channel();
        let worker_id = id.clone();
        std::thread::spawn(move || {
            let _ = tx.send(plugins::activate(&Paths::discover(), &worker_id, &token).map_err(|e| e.to_string()));
        });
        let window = window.clone();
        let status = status.clone();
        status.set_text("Running plugin action…");
        glib::timeout_add_local(Duration::from_millis(25), move || {
            if let Ok(reply) = rx.try_recv() {
                match reply {
                    Ok(action) => activate(
                        &ResultRow {
                            title: String::new(),
                            folder: String::new(),
                            reason: String::new(),
                            action: Action::Plugin(id.clone(), result_id.clone(), action),
                        },
                        &window,
                        &status,
                        &RefCell::new(None),
                        reveal,
                    ),
                    Err(e) => status.set_text(&e),
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        return;
    }
    let p = Paths::discover();
    let action = result.action.clone();
    let perform = (|| -> anyhow::Result<()> {
        match action {
            Action::Copy(text) => window.clipboard().set_text(&text),
            Action::App(id) => {
                gio::AppInfo::all()
                    .into_iter()
                    .find(|a| a.id().is_some_and(|x| x == id))
                    .ok_or_else(|| anyhow::anyhow!("application unavailable"))?
                    .launch(&[], None::<&gio::AppLaunchContext>)?;
            }
            Action::File(path, hit) => {
                if !config::allowed(&p.load()?, &path) {
                    anyhow::bail!("source no longer permitted");
                }
                open_file(&path, &hit, reveal)?;
            }
            Action::System(cmd) => {
                if cmd != "lock" && armed.borrow().as_deref() != Some(cmd) {
                    *armed.borrow_mut() = Some(cmd.into());
                    status.set_text("Press Enter again to confirm");
                    return Ok(());
                }
                Command::new("omarchy").args(["system", cmd]).spawn()?;
                *armed.borrow_mut() = None;
            }
            Action::Plugin(id, _, action) => {
                let plugin = plugins::get(&p, &id)?;
                if !plugin.grant.enabled || !plugins::valid_action(&p, &plugin, &action) {
                    anyhow::bail!("plugin permission revoked");
                }
                let action = if let plugins::Action::Activate { token } = action {
                    if let Some(flag) = plugins::capture_host_args(&id, &token) {
                        plugins::spawn_capture(flag)?;
                        return Ok(());
                    }
                    plugins::activate(&p, &id, &token)?
                } else {
                    action
                };
                match action {
                    plugins::Action::Copy { text } => window.clipboard().set_text(&text),
                    plugins::Action::Open { path } => open_file(&path, &Value::Null, reveal)?,
                    plugins::Action::Url { url } => gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)?,
                    plugins::Action::Activate { .. } => unreachable!(),
                }
            }
            // Routed to `pw::activate` before this; nothing to do here.
            Action::Password(_) => {}
        }
        Ok(())
    })();
    match perform {
        Err(e) => status_error(status, e),
        Ok(()) => {
            if armed.borrow().is_none() {
                window.set_visible(false);
            }
        }
    }
}
fn open_file(path: &Path, hit: &Value, reveal: bool) -> anyhow::Result<()> {
    if reveal {
        let parent = path.parent().unwrap_or(path);
        gio::AppInfo::launch_default_for_uri(&gio::File::for_path(parent).uri(), None::<&gio::AppLaunchContext>)?;
        return Ok(());
    }
    let uri = gio::File::for_path(path).uri();
    let content_type = content_type(path);
    if let Some(app) = gio::AppInfo::default_for_type(&content_type, false)
        && let Some(id) = app.id()
    {
        let page = hit["info"]["page"]
            .as_u64()
            .or_else(|| hit["loc"].as_f64().filter(|_| hit["kind"] == "pdf").map(|n| n as u64))
            .unwrap_or(1)
            .max(1);
        let moment = hit["info"]["seconds"]
            .as_f64()
            .or_else(|| hit["loc"].as_f64().filter(|_| hit["kind"] == "audio" || hit["kind"] == "video"))
            .filter(|n| n.is_finite() && *n >= 0.);
        let line = hit["info"]["line"]
            .as_u64()
            .or_else(|| hit["focus"].as_u64())
            .or_else(|| hit["loc"].as_f64().filter(|_| hit["segment"] == "lines").map(|n| n as u64));
        let mut special: Option<Command> = None;
        if content_type == "application/pdf" {
            special = match id.as_str() {
                "io.github.design_nexus.nixe.Pdf.desktop" => {
                    let mut c = Command::new("nixe-pdf");
                    c.args(["--page", &page.to_string()]);
                    Some(c)
                }
                "org.gnome.Evince.desktop" => {
                    let mut c = Command::new("evince");
                    c.arg(format!("--page-label={page}"));
                    Some(c)
                }
                "org.kde.okular.desktop" => {
                    let mut c = Command::new("okular");
                    c.args(["--page", &page.to_string()]);
                    Some(c)
                }
                "org.pwmt.zathura.desktop" => {
                    let mut c = Command::new("zathura");
                    c.args(["-P", &page.to_string()]);
                    Some(c)
                }
                _ => None,
            };
        }
        if let Some(seconds) = moment {
            special = match id.as_str() {
                "mpv.desktop" => {
                    let mut c = Command::new("mpv");
                    c.arg(format!("--start={seconds}"));
                    Some(c)
                }
                "vlc.desktop" | "org.videolan.VLC.desktop" => {
                    let mut c = Command::new("vlc");
                    c.arg(format!("--start-time={seconds}"));
                    Some(c)
                }
                _ => None,
            };
        }
        if let Some(line) = line {
            match id.as_str() {
                "code.desktop" | "code-url-handler.desktop" | "codium.desktop" => {
                    let mut c = Command::new(if id.starts_with("codium") { "codium" } else { "code" });
                    c.args(["-g", &format!("{}:{line}", path.display())]).spawn()?;
                    return Ok(());
                }
                "dev.zed.Zed.desktop" => {
                    Command::new("zed").arg(format!("{}:{line}", path.display())).spawn()?;
                    return Ok(());
                }
                _ => {}
            }
        }
        if let Some(mut cmd) = special {
            cmd.arg(path).spawn()?;
            return Ok(());
        }
    }
    gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)?;
    Ok(())
}

fn highlight(text: &str, query: &str) -> String {
    let keyword = query.split_whitespace().find(|s| s.chars().count() >= 3).unwrap_or("");
    if keyword.is_empty() {
        return glib::markup_escape_text(text).to_string();
    }
    // Match regardless of case, but show the text as it is written.
    let needle: Vec<char> = keyword.chars().flat_map(char::to_lowercase).collect();
    let match_at = |start: usize| {
        let mut matched = 0;
        for (offset, ch) in text[start..].char_indices() {
            for lower in ch.to_lowercase() {
                if needle.get(matched) != Some(&lower) {
                    return None;
                }
                matched += 1;
            }
            if matched == needle.len() {
                return Some(start + offset + ch.len_utf8());
            }
        }
        None
    };
    let mut out = String::new();
    let (mut plain, mut at) = (0, 0);
    while let Some(ch) = text[at..].chars().next() {
        if let Some(end) = match_at(at) {
            out.push_str(&glib::markup_escape_text(&text[plain..at]));
            out.push_str(&format!("<b>{}</b>", glib::markup_escape_text(&text[at..end])));
            (plain, at) = (end, end);
        } else {
            at += ch.len_utf8();
        }
    }
    out.push_str(&glib::markup_escape_text(&text[plain..]));
    out
}
fn close_controls(window: &gtk::ApplicationWindow, root: &gtk::Box) {
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    let weak = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(w) = weak.upgrade() {
            w.close();
        }
    });
    root.append(&close);
    if window.has_css_class("escape-close") {
        return;
    }
    window.add_css_class("escape-close");
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = weak.upgrade() {
                w.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
}
fn task_window(app: &gtk::Application, title: &str) -> (gtk::ApplicationWindow, gtk::Label) {
    let parent = gtk::ApplicationWindow::builder().application(app).title(title).default_width(500).default_height(180).build();
    parent.add_css_class("glimpse-window");
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("glimpse-dialog");
    root.set_margin_top(16);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_bottom(16);
    let label = gtk::Label::new(Some(title));
    label.set_wrap(true);
    root.append(&label);
    close_controls(&parent, &root);
    parent.set_child(Some(&root));
    (parent, label)
}
pub fn add_folder(app: &gtk::Application) {
    dismiss();
    let (parent, label) = task_window(app, "Glimpse · Add folder");
    parent.present();
    let dialog = gtk::FileDialog::builder().title("Choose a folder for Glimpse").build();
    dialog.select_folder(Some(&parent.clone()), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            parent.present();
            run_task(label, move || {
                nixe_glimpse::cli::dispatch(&["sources".into(), "add".into(), path.display().to_string()])?;
                Ok("Folder added. Background indexing is enabled.".into())
            });
        } else {
            parent.close();
        }
    });
}
pub fn add_plugin(app: &gtk::Application) {
    dismiss();
    let (parent, label) = task_window(app, "Glimpse · Add plugin");
    parent.present();
    let dialog = gtk::FileDialog::builder().title("Choose a plugin folder").build();
    dialog.select_folder(Some(&parent.clone()), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            parent.present();
            run_task(label, move || {
                plugins::install_local(&Paths::discover(), &path, &format!("local:{}", path.display()), "", false)?;
                Ok("Plugin installed, disabled. Review its permissions in Plugins.".into())
            });
        } else {
            parent.close();
        }
    });
}
fn app_keywords(id: &str) -> String {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    let mut dirs = vec![data];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
    ));
    for dir in dirs {
        let key = glib::KeyFile::new();
        if key.load_from_file(dir.join("applications").join(id), glib::KeyFileFlags::NONE).is_ok() {
            return key
                .string_list("Desktop Entry", "Keywords")
                .map(|v| v.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
        }
    }
    String::new()
}
pub fn plugin_manager(app: &gtk::Application) {
    dismiss();
    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Glimpse · Plugins")
        .default_width(800)
        .default_height(580)
        .build();
    win.add_css_class("glimpse-window");
    rebuild_plugin_manager(app, &win);
    let weak = win.downgrade();
    let app = app.clone();
    let mut snapshot = plugin_snapshot();
    glib::timeout_add_local(Duration::from_millis(500), move || {
        let Some(win) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let fresh = plugin_snapshot();
        if fresh != snapshot {
            snapshot = fresh;
            rebuild_plugin_manager(&app, &win);
        }
        glib::ControlFlow::Continue
    });
    win.present();
}
fn plugin_snapshot() -> Vec<Vec<u8>> {
    let p = Paths::discover();
    [p.config.join("plugins.json"), p.data.join("plugin-catalog.json"), p.state.join("catalog-status.txt")]
        .iter()
        .map(|path| std::fs::read(path).unwrap_or_default())
        .collect()
}
fn rebuild_plugin_manager(app: &gtk::Application, win: &gtk::ApplicationWindow) {
    // Keep the selected tab when actions update the displayed lists.
    let tab = win
        .child()
        .and_then(|root| root.last_child())
        .and_then(|child| child.downcast::<gtk::Stack>().ok())
        .and_then(|s| s.visible_child_name());
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("glimpse-dialog");
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);
    let status = gtk::Label::new(Some("Plugins run separately. Review permissions before enabling."));
    status.set_wrap(true);
    let catalog_status = std::fs::read_to_string(Paths::discover().state.join("catalog-status.txt")).unwrap_or_default();
    if !catalog_status.is_empty() {
        status.set_text(&catalog_status);
    }
    root.append(&status);
    close_controls(win, &root);
    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let local = gtk::Button::with_label("Add local plugin");
    let app_clone = app.clone();
    local.connect_clicked(move |_| add_plugin(&app_clone));
    tools.append(&local);
    let refresh = gtk::Button::with_label("Refresh catalog");
    let st = status.clone();
    refresh.connect_clicked(move |_| {
        run_task(st.clone(), || {
            let p = Paths::discover();
            plugins::catalog(&p, true)?;
            Ok(std::fs::read_to_string(p.state.join("catalog-status.txt"))?)
        })
    });
    tools.append(&refresh);
    root.append(&tools);
    let stack = gtk::Stack::new();
    let switch = gtk::StackSwitcher::builder().stack(&stack).build();
    root.append(&switch);
    let installed = gtk::Box::new(gtk::Orientation::Vertical, 8);
    if let Ok(items) = plugins::list(&Paths::discover()) {
        for plugin in items {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
            let text = gtk::Label::new(Some(&format!(
                "{} {} · {}\n{}\nNetwork: {} · Read folders: {}",
                plugin.manifest.name,
                plugin.manifest.version,
                if plugin.grant.enabled { "enabled" } else { "disabled" },
                plugin.manifest.description,
                plugin.manifest.permissions.network,
                if plugin.grant.enabled {
                    plugin.grant.permissions.read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                } else if plugin.manifest.permissions.selected_sources {
                    Paths::discover()
                        .load()
                        .map(|c| c.roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
                        .unwrap_or_default()
                } else {
                    plugin.manifest.permissions.read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                }
            )));
            text.set_xalign(0.);
            text.set_wrap(true);
            row.append(&text);
            if let Some(error) = plugins::last_error(&Paths::discover(), &plugin.manifest.id) {
                let label = gtk::Label::new(Some(&format!("Last error: {error}")));
                label.add_css_class("dim-label");
                label.set_xalign(0.);
                label.set_wrap(true);
                row.append(&label);
            }
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            for (label, op) in [
                (if plugin.grant.enabled { "Disable" } else { "Enable · approve listed permissions" }, "toggle"),
                ("Update", "update"),
                ("Rollback", "rollback"),
                ("Remove", "remove"),
            ] {
                let id = plugin.manifest.id.clone();
                let enabled = plugin.grant.enabled;
                let st = status.clone();
                let run = move || {
                    let id = id.clone();
                    run_task(st.clone(), move || {
                        let p = Paths::discover();
                        match op {
                            "toggle" => {
                                if enabled {
                                    plugins::disable(&p, &id)?;
                                } else {
                                    plugins::enable(&p, &id, true)?;
                                }
                            }
                            "update" => {
                                plugins::update(&p, &id)?;
                            }
                            "rollback" => plugins::rollback(&p, &id)?,
                            "remove" => plugins::remove(&p, &id, false)?,
                            _ => {}
                        }
                        Ok("Plugin updated.".into())
                    });
                };
                // Removing deletes the package, so it takes the suite's two clicks.
                let button = if op == "remove" {
                    nixe_ui::widgets::two_click(label, "Click again to remove", run)
                } else {
                    let button = gtk::Button::with_label(label);
                    button.connect_clicked(move |_| run());
                    button
                };
                actions.append(&button);
            }
            row.append(&actions);
            if plugin.manifest.capabilities.contains(&"settings".into()) {
                let button = gtk::Button::with_label("Settings");
                let app_clone = app.clone();
                let id = plugin.manifest.id.clone();
                button.connect_clicked(move |_| plugin_settings(&app_clone, &id));
                row.append(&button);
            }
            installed.append(&row);
        }
    }
    if installed.first_child().is_none() {
        let empty = gtk::Label::new(Some("No plugins installed. Open Browse or add a local plugin."));
        empty.set_wrap(true);
        installed.append(&empty);
    }
    let installed_scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hscrollbar_policy(gtk::PolicyType::External)
        .child(&installed)
        .vexpand(true)
        .build();
    stack.add_titled(&installed_scroll, Some("installed"), "Installed");
    let browse = gtk::Box::new(gtk::Orientation::Vertical, 8);
    if let Ok(catalog) = plugins::catalog(&Paths::discover(), false) {
        for item in catalog {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let text = gtk::Label::new(Some(&format!("{} · {}", item.name, item.description)));
            text.set_hexpand(true);
            text.set_wrap(true);
            text.set_xalign(0.);
            row.append(&text);
            let install = gtk::Button::with_label("Install");
            let st = status.clone();
            install.connect_clicked(move |_| {
                let item = item.clone();
                run_task(st.clone(), move || {
                    let p = Paths::discover();
                    if item.id == "quick-tools" {
                        plugins::install_builtin(&p)?;
                    } else {
                        plugins::install_git(&p, &item.repo, &item.revision, &item.folder, false)?;
                    }
                    Ok("Installed, disabled. Review permissions in Installed.".into())
                });
            });
            row.append(&install);
            browse.append(&row);
        }
    }
    let git = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let url = gtk::Entry::builder().placeholder_text("HTTPS Git repository").build();
    let revision = gtk::Entry::builder().placeholder_text("Full 40-character commit hash").build();
    let folder = gtk::Entry::builder().placeholder_text("Plugin subfolder (optional)").build();
    git.append(&url);
    git.append(&revision);
    git.append(&folder);
    let install = gtk::Button::with_label("Install from Git");
    let st = status.clone();
    install.connect_clicked(move |_| {
        let (u, r, f) = (url.text().to_string(), revision.text().to_string(), folder.text().to_string());
        run_task(st.clone(), move || {
            plugins::install_git(&Paths::discover(), &u, &r, &f, false)?;
            Ok("Installed, disabled. Review permissions in Installed.".into())
        });
    });
    git.append(&install);
    browse.append(&git);
    let browse_scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hscrollbar_policy(gtk::PolicyType::External)
        .child(&browse)
        .vexpand(true)
        .build();
    stack.add_titled(&browse_scroll, Some("browse"), "Browse");
    root.append(&stack);
    win.set_child(Some(&root));
    if let Some(tab) = tab {
        stack.set_visible_child_name(&tab);
    }
}
fn run_task(label: gtk::Label, task: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
    label.set_text("Working…");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(task().map_err(|e| e.to_string()));
    });
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if let Ok(result) = rx.try_recv() {
            label.set_text(&result.unwrap_or_else(|e| e));
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}
type ValueGetter = Rc<dyn Fn() -> Value>;
fn setting_control(row: &Value) -> Option<(gtk::Widget, ValueGetter)> {
    match row["kind"].as_str().unwrap_or("entry") {
        "entry" => {
            let entry = gtk::Entry::builder().text(row["value"].as_str().unwrap_or("")).build();
            let read = entry.clone();
            Some((entry.upcast(), Rc::new(move || json!(read.text().to_string()))))
        }
        "switch" => {
            let switch = gtk::Switch::builder().active(row["value"].as_bool().unwrap_or(false)).build();
            let read = switch.clone();
            Some((switch.upcast(), Rc::new(move || json!(read.is_active()))))
        }
        "slider" => {
            let (min, max, step) = (row["min"].as_f64()?, row["max"].as_f64()?, row["step"].as_f64().unwrap_or(1.));
            if !min.is_finite() || !max.is_finite() || !step.is_finite() || min >= max || step <= 0. {
                return None;
            }
            let spin = gtk::SpinButton::with_range(min, max, step);
            spin.set_value(row["value"].as_f64().unwrap_or(min).clamp(min, max));
            let read = spin.clone();
            Some((spin.upcast(), Rc::new(move || json!(read.value()))))
        }
        "choice" => {
            let options: Vec<(String, String)> = serde_json::from_value(row["options"].clone()).ok()?;
            if options.is_empty() || options.len() > 128 {
                return None;
            }
            let labels = options.iter().map(|(_, label)| label.as_str()).collect::<Vec<_>>();
            let dropdown = gtk::DropDown::from_strings(&labels);
            dropdown
                .set_selected(options.iter().position(|(id, _)| Some(id.as_str()) == row["value"].as_str()).unwrap_or(0) as u32);
            let read = dropdown.clone();
            Some((
                dropdown.upcast(),
                Rc::new(move || json!(options.get(read.selected() as usize).map(|(id, _)| id).cloned().unwrap_or_default())),
            ))
        }
        _ => None,
    }
}
fn plugin_settings(app: &gtk::Application, id: &str) {
    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .title(format!("Glimpse · {id} settings"))
        .default_width(600)
        .default_height(400)
        .build();
    win.add_css_class("glimpse-window");
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("glimpse-dialog");
    root.set_margin_top(16);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_bottom(16);
    let status = gtk::Label::new(Some("Loading settings…"));
    status.set_wrap(true);
    root.append(&status);
    close_controls(&win, &root);
    let scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hscrollbar_policy(gtk::PolicyType::External)
        .child(&root)
        .build();
    win.set_child(Some(&scroll));
    win.present();
    let id = id.to_owned();
    let (tx, rx) = mpsc::channel();
    let worker_id = id.clone();
    std::thread::spawn(move || {
        let _ =
            tx.send(plugins::call(&Paths::discover(), &worker_id, "settings", "settings", json!({})).map_err(|e| e.to_string()));
    });
    glib::timeout_add_local(Duration::from_millis(50), move || {
        if let Ok(reply) = rx.try_recv() {
            match reply {
                Err(e) => status.set_text(&e),
                Ok(value) => {
                    status.set_text("Plugin settings");
                    if let Some(rows) = value["rows"].as_array() {
                        for row in rows.iter().take(32) {
                            let Some(key) = row["key"].as_str() else {
                                continue;
                            };
                            if key.len() > 128 {
                                continue;
                            }
                            let title = row["title"].as_str().unwrap_or(key);
                            let description = row["desc"].as_str().unwrap_or("");
                            let Some((control, read)) = setting_control(row) else {
                                root.append(&gtk::Label::new(Some(title)));
                                continue;
                            };
                            root.append(&nixe_ui::widgets::row(title, description, Some(&control)));
                            let button = gtk::Button::with_label(&format!("Save {title}"));
                            let id = id.clone();
                            let key = key.to_owned();
                            let label = status.clone();
                            button.connect_clicked(move |_| {
                                let value = read();
                                let id = id.clone();
                                let key = key.clone();
                                run_task(label.clone(), move || {
                                    plugins::call(
                                        &Paths::discover(),
                                        &id,
                                        "settings",
                                        "configure",
                                        json!({"key":key,"value":value}),
                                    )?;
                                    Ok("Saved".into())
                                });
                            });
                            root.append(&button);
                        }
                    }
                }
            }
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn stop_playback(slot: &gtk::Box, playback: &RefCell<Option<Player>>) {
    if let Some(player) = playback.borrow_mut().take() {
        slot.remove(player.widget());
    }
}
/// The file's type from its name, except that a `.ts` file which isn't an MPEG transport
/// stream is TypeScript, so it neither looks like nor opens as a video.
fn content_type(path: &Path) -> String {
    let typescript = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ts")) && media_kind(path).is_none();
    if typescript {
        return "application/typescript".into();
    }
    gio::content_type_guess(Some(path), None::<&[u8]>).0.to_string()
}
fn media_kind(path: &Path) -> Option<bool> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ["mp4", "mkv", "webm", "mov", "avi", "m4v", "mpeg", "mpg", "ogv"].contains(&ext.as_str()) {
        Some(true)
    } else if ext == "ts" {
        // .ts is far more often TypeScript; a transport stream starts with the 0x47 sync byte.
        let mut first = [0u8; 1];
        let is_stream = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut first)).is_ok() && first[0] == 0x47;
        is_stream.then_some(true)
    } else if ["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac", "aiff", "wma"].contains(&ext.as_str()) {
        Some(false)
    } else {
        None
    }
}
/// Syntax colours drawn from the Nixe palette rather than a fixed editor theme, so code
/// previews match every theme: the accent for structure, dim italics for comments.
fn code_markup(code: &str, path: &Path) -> Option<String> {
    use std::{str::FromStr, sync::OnceLock};
    use syntect::{
        easy::HighlightLines,
        highlighting::{Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings},
        parsing::SyntaxSet,
        util::LinesWithEndings,
    };
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    let ss = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let syntax = ss.find_syntax_by_extension(path.extension()?.to_str()?)?;
    if syntax.name == "Plain Text" {
        return None;
    }
    let palette = nixe_theme::current_palette();
    let color = |hex: &str| {
        let v = u32::from_str_radix(hex.trim_start_matches('#').get(..6)?, 16).ok()?;
        Some(Color { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 255 })
    };
    let item = |scope: &str, hex: &str, style: FontStyle| ThemeItem {
        scope: ScopeSelectors::from_str(scope).unwrap_or_default(),
        style: StyleModifier { foreground: color(hex), background: None, font_style: Some(style) },
    };
    let (accent, dim) = (palette.accent.as_str(), palette.dim_text.as_str());
    let theme = Theme {
        settings: ThemeSettings { foreground: color(&palette.text), ..Default::default() },
        scopes: vec![
            item("comment, punctuation.definition.comment, markup.quote", dim, FontStyle::ITALIC),
            item(
                "keyword, storage, constant.numeric, constant.language, entity.name.type, support.type",
                accent,
                FontStyle::empty(),
            ),
            item("markup.heading, entity.name.section, punctuation.definition.heading", accent, FontStyle::BOLD),
            item(
                "markup.raw, markup.inline.raw, markup.underline.link, punctuation.definition.list_item",
                accent,
                FontStyle::empty(),
            ),
            item("entity.name.function, markup.bold", &palette.text, FontStyle::BOLD),
            item("markup.italic", &palette.text, FontStyle::ITALIC),
        ],
        ..Default::default()
    };
    let mut highlighter = HighlightLines::new(syntax, &theme);
    let mut markup = String::new();
    for line in LinesWithEndings::from(code) {
        for (style, token) in highlighter.highlight_line(line, ss).ok()? {
            let c = style.foreground;
            let weight = if style.font_style.contains(FontStyle::BOLD) { " weight=\"bold\"" } else { "" };
            let slant = if style.font_style.contains(FontStyle::ITALIC) { " style=\"italic\"" } else { "" };
            markup.push_str(&format!(
                "<span foreground=\"#{:02x}{:02x}{:02x}\"{weight}{slant}>{}</span>",
                c.r,
                c.g,
                c.b,
                glib::markup_escape_text(token)
            ));
        }
    }
    Some(markup)
}
pub fn dismiss() {
    PANEL.with(|p| {
        if let Some(window) = p.borrow().as_ref() {
            window.set_visible(false);
        }
    });
}

/// Select a result by index and scroll just enough to show it.
fn select_and_reveal(list: &gtk::ListBox, scroll: &gtk::ScrolledWindow, browsing: &std::cell::Cell<bool>, index: i32) {
    let Some(row) = list.row_at_index(index) else { return };
    browsing.set(true);
    list.select_row(Some(&row));
    if let Some(bounds) = row.compute_bounds(list) {
        let adj = scroll.vadjustment();
        let y = bounds.y() as f64;
        let bottom = y + bounds.height() as f64;
        if y < adj.value() {
            adj.set_value(y.max(0.));
        } else if bottom > adj.value() + adj.page_size() {
            adj.set_value((bottom - adj.page_size()).min(adj.upper() - adj.page_size()).max(0.));
        }
    }
}
fn navigate_result(list: &gtk::ListBox, direction: i32) {
    let next = list.selected_row().map(|row| (row.index() + direction).max(0)).unwrap_or(0);
    if let Some(row) = list.row_at_index(next) {
        list.select_row(Some(&row));
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn highlight_ignores_case_and_keeps_text() {
        assert_eq!(super::highlight("The Report & report", "report"), "The <b>Report</b> &amp; <b>report</b>");
        assert_eq!(super::highlight("Ärger ÄRGER", "ärger"), "<b>Ärger</b> <b>ÄRGER</b>");
        assert_eq!(super::highlight("abc", "zz"), "abc");
    }
}
