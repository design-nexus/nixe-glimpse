use gtk::{gdk, gio, glib, prelude::*};
use nixe_glimpse::{
    config::{self, Paths},
    index, plugins, query, semantic, utility,
};
use crate::player::Player;
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
thread_local! {static PANEL:RefCell<Option<gtk::ApplicationWindow>>=const{RefCell::new(None)};}
#[derive(Clone)]
enum Action {
    App(String),
    File(PathBuf, Value),
    Copy(String),
    System(&'static str),
    Plugin(String, String, plugins::Action),
}
#[derive(Clone)]
struct ResultRow {
    title: String,
    detail: String,
    action: Action,
}
impl ResultRow {
    fn category(&self) -> (u8, &'static str) {
        match self.action {
            Action::App(_) => (0, "Applications"),
            Action::File(_, ref hit) if hit["related"] == true => (5, "Related results"),
            Action::File(_, _) => (1, "Files"),
            Action::Copy(_) => (2, "Utilities"),
            Action::System(_) => (3, "System Commands"),
            Action::Plugin(_, _, _) => (4, "Plugins"),
        }
    }
    fn identity(&self) -> String {
        match &self.action {
            Action::App(id) => format!("app:{id}"),
            Action::File(path, _) => format!("file:{}", path.display()),
            Action::Copy(value) => format!("copy:{value}"),
            Action::System(cmd) => format!("system:{cmd}"),
            Action::Plugin(id, hit, _) => format!("plugin:{id}:{hit}"),
        }
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
        .default_width(640)
        .default_height(64)
        .decorated(false)
        .resizable(false)
        .build();
    window.add_css_class("glimpse-window");
    let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main.add_css_class("glimpse-frame");
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toolbar.add_css_class("search-toolbar");
    let entry = gtk::SearchEntry::builder().placeholder_text("Find apps, files, ideas…").search_delay(0).hexpand(true).build();
    toolbar.append(&entry);
    let folder = nixe_ui::widgets::icon_button("folder-new-symbolic", "Add a folder");
    let app_clone = app.clone();
    folder.connect_clicked(move |_| add_folder(&app_clone));
    toolbar.append(&folder);
    let plugin = nixe_ui::widgets::icon_button("application-x-addon-symbolic", "Plugins");
    let app_clone = app.clone();
    plugin.connect_clicked(move |_| plugin_manager(&app_clone));
    toolbar.append(&plugin);
    folder.set_visible(false);
    plugin.set_visible(false);
    main.append(&toolbar);
    let content = gtk::Paned::new(gtk::Orientation::Horizontal);
    content.set_position(350);
    content.set_size_request(-1, 550);
    content.set_vexpand(true);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    let scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hscrollbar_policy(gtk::PolicyType::External)
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::External)
        .build();
    content.set_start_child(Some(&scroll));
    let preview_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    preview_box.add_css_class("preview");
    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_size_request(180, 180);
    picture.set_visible(false);
    let preview = gtk::Label::new(Some("Select a result to preview it."));
    preview.set_wrap(true);
    preview.set_selectable(true);
    preview.set_xalign(0.);
    preview.set_yalign(0.);
    let preview_scroll = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hscrollbar_policy(gtk::PolicyType::External)
        .child(&preview)
        .vexpand(true)
        .build();
    let media = gtk::Box::new(gtk::Orientation::Vertical, 0);
    preview_box.append(&media);
    preview_box.append(&picture);
    let playback: Rc<RefCell<Option<Player>>> = Rc::new(RefCell::new(None));
    let media_hide = media.clone();
    let playback_hide = playback.clone();
    window.connect_visible_notify(move |w| {
        if !w.is_visible() {
            stop_playback(&media_hide, &playback_hide);
            let weak = w.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(w) = weak.upgrade() && !w.is_visible() && let Some(app) = w.application()
                    && !app.windows().iter().any(|other| other.is_visible())
                {
                    // A dismissed launcher should release its native surface and D-Bus name.
                    app.quit();
                }
            });
        }
    });
    preview_box.append(&preview_scroll);
    let (calculator, calculation_display) = calculator_preview(&entry);
    calculator.set_visible(false);
    preview_box.append(&calculator);
    content.set_end_child(Some(&preview_box));
    content.set_visible(false);
    main.append(&content);
    let footer = gtk::Label::new(Some("↑ ↓ select · Enter open · Ctrl+Enter reveal · Esc close"));
    footer.set_xalign(0.);
    footer.add_css_class("footer");
    footer.set_visible(false);
    main.append(&footer);
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
            let label = gtk::Label::new(Some(result.category().1));
            label.set_xalign(0.);
            label.add_css_class("category-heading");
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
                    detail: format!("{} · {}", id, hit.subtitle),
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
                                    detail: format!(
                                        "{} · {}",
                                        if hit["related"] == true { "Related" } else { "Content match" },
                                        path.display()
                                    ),
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
    let gen_worker = generation.clone();
    std::thread::spawn(move || {
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
                        error = "↑ ↓ select · Enter copy · Esc close".into();
                        found.extend(answers.into_iter().map(|answer| ResultRow {
                            title: answer.title,
                            detail: answer.detail,
                            action: Action::Copy(answer.copy),
                        }));
                    }
                    Err(e) => error = e.to_string(),
                }
                let _ = tx.send(Reply { generation: request_generation, rows: found, append: false, status: error });
                continue;
            }
            let lower = q.to_lowercase();
            let mut matching_apps: Vec<_> = apps
                .iter()
                .filter_map(|(id, name, keywords)| {
                    let rank = query::name_rank(name, q).or_else(|| query::name_rank(keywords, q).map(|r| r + 3))?;
                    Some((rank, name, id))
                })
                .collect();
            matching_apps.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
            for (_, name, id) in matching_apps.into_iter().take(12) {
                found.push(ResultRow { title: name.clone(), detail: "Application".into(), action: Action::App(id.clone()) });
            }
            if let Ok(files) = index::search(&p, &c, q, 30) {
                for f in files {
                    found.push(ResultRow {
                        title: f.name,
                        detail: f.path.display().to_string(),
                        action: Action::File(f.path, Value::Null),
                    });
                }
            }
            if let Ok(hits) = index::content_search(&p, &c, q) {
                for (file, info) in hits {
                    found.push(ResultRow {
                        title: file.name,
                        detail: format!("Reader match · {}", file.path.display()),
                        action: Action::File(file.path, info),
                    });
                }
            }
            for (name, cmd) in [("Lock screen", "lock"), ("Reboot", "reboot"), ("Shut down", "shutdown")] {
                if !q.is_empty() && name.to_lowercase().contains(&lower) {
                    found.push(ResultRow { title: name.into(), detail: "System action".into(), action: Action::System(cmd) });
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
                && event.paths.iter().any(|path| {
                    path.file_name().is_some_and(|name| {
                        ["settings.toml", "plugins.json"].iter().any(|s| name == *s)
                    })
                })
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
    let gen_poll = generation.clone();
    let preview_poll = preview_box.clone();
    let window_weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(25), move || {
        if window_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        if invalidated.swap(false, Ordering::Relaxed) {
            preview_poll.set_visible(Paths::discover().load().is_ok_and(|c| c.preview));
            let g = gen_poll.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = refresh_jobs.send((g, refresh_entry.text().to_string()));
        }
        while let Ok(reply) = rx.try_recv() {
            if reply.generation != gen_poll.load(Ordering::Relaxed) {
                continue;
            }
            // Late enrichment must never move the list under keyboard or mouse navigation.
            if browsing_poll.get() {
                continue;
            }
            if !reply.status.is_empty() {
                footer_poll.set_text(&reply.status);
            }
            let selected =
                list_poll.selected_row().and_then(|r| rows_poll.borrow().get(r.index() as usize).map(ResultRow::identity));
            let mut merged = if reply.append { rows_poll.borrow().clone() } else { Vec::new() };
            for result in reply.rows {
                if let Some(existing) = merged.iter_mut().find(|r| r.identity() == result.identity()) {
                    if matches!(&result.action, Action::File(_, hit) if !hit.is_null()) {
                        let mut result = result;
                        if existing.category().1 != "Related results"
                            && let Action::File(_, ref mut hit) = result.action
                        {
                            hit["related"] = false.into();
                        }
                        *existing = result;
                    }
                } else {
                    merged.push(result);
                }
            }
            // A lexical refresh arrives before enrichment. Keep the selected
            // semantic/plugin result until that stage catches up.
            if let Some(id) = selected.as_ref()
                && !merged.iter().any(|r| &r.identity() == id)
                && let Some(old) = rows_poll.borrow().iter().find(|r| &r.identity() == id)
                && !matches!(&old.action, Action::File(path, _) if !path.exists())
            {
                merged.push(old.clone());
            }
            merged.sort_by_key(|r| r.category().0);
            let unchanged = {
                let old = rows_poll.borrow();
                old.len() == merged.len() && old.iter().zip(&merged).all(|(a, b)| {
                    a.identity() == b.identity() && a.title == b.title && a.detail == b.detail
                })
            };
            if unchanged {
                *rows_poll.borrow_mut() = merged;
                continue;
            }
            let scroll_position = scroll_poll.vadjustment().value();
            rebuild_poll.set(true);
            while let Some(child) = list_poll.first_child() {
                list_poll.remove(&child);
            }
            *rows_poll.borrow_mut() = merged.clone();
            for result in &merged {
                let line = gtk::Box::new(gtk::Orientation::Vertical, 2);
                line.add_css_class("result");
                let title = gtk::Label::new(Some(&result.title));
                title.set_xalign(0.);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let detail = gtk::Label::new(Some(&result.detail));
                detail.add_css_class("dim-label");
                detail.set_xalign(0.);
                detail.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                line.append(&title);
                line.append(&detail);
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&line));
                list_poll.append(&row);
            }
            let selected_index = selected.and_then(|id| merged.iter().position(|r| r.identity() == id)).unwrap_or(0);
            list_poll.select_row(list_poll.row_at_index(selected_index as i32).as_ref());
            rebuild_poll.set(false);
            // Notify once, after the selected file has been restored.
            list_poll.emit_by_name::<()>("row-selected", &[&list_poll.selected_row()]);
            let adjustment = scroll_poll.vadjustment();
            glib::idle_add_local_once(move || adjustment.set_value(scroll_position));
            if merged.is_empty() && reply.status.is_empty() {
                footer_poll.set_text("No results");
            }
        }
        glib::ControlFlow::Continue
    });
    let jobs_entry = jobs.clone();
    let gen_entry = generation.clone();
    let footer_entry = footer.clone();
    let armed_entry = armed.clone();
    let content_entry = content.clone();
    let preview_entry = preview_box.clone();
    let calculator_entry = calculator.clone();
    let preview_scroll_entry = preview_scroll.clone();
    let picture_entry = picture.clone();
    let media_entry = media.clone();
    let playback_entry = playback.clone();
    let display_entry = calculation_display.clone();
    let window_entry = window.clone();
    let list_entry = list.clone();
    let rows_entry = rows.clone();
    let browsing_entry = browsing.clone();
    entry.connect_search_changed(move |e| {
        browsing_entry.set(false);
        let expanded = !e.text().trim().is_empty();
        let is_math = utility::intent(&e.text()) == utility::Intent::Calculation;
        calculator_entry.set_visible(is_math);
        preview_scroll_entry.set_visible(!is_math);
        picture_entry.set_visible(false);
        stop_playback(&media_entry, &playback_entry);
        if is_math {
            let c = Paths::discover().load().unwrap_or_default();
            display_entry.set_text(
                &utility::answers(&e.text(), &c)
                    .ok()
                    .and_then(|a| a.into_iter().next())
                    .map(|a| a.title)
                    .unwrap_or_else(|| utility::pretty_math(&e.text())),
            );
        }
        content_entry.set_visible(expanded);
        footer_entry.set_visible(expanded);
        folder.set_visible(expanded);
        plugin.set_visible(expanded);
        preview_entry.set_visible(Paths::discover().load().is_ok_and(|c| c.preview));
        window_entry.set_default_size(if expanded { 940 } else { 640 }, if expanded { 660 } else { 64 });
        while let Some(child) = list_entry.first_child() {
            list_entry.remove(&child);
        }
        rows_entry.borrow_mut().clear();
        *armed_entry.borrow_mut() = None;
        footer_entry.set_text("↑ ↓ select · Enter open · Ctrl+Enter reveal · Esc close");
        let g = gen_entry.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = jobs_entry.send((g, e.text().to_string()));
    });
    let preview_gen = Arc::new(AtomicU64::new(0));
    let (preview_tx, preview_rx) = mpsc::channel();
    let rows_preview = rows.clone();
    let gen_preview = preview_gen.clone();
    let query_entry = entry.clone();
    let armed_preview = armed.clone();
    let preview_label = preview.clone();
    let selected_preview = Rc::new(RefCell::new(None::<String>));
    let picture_select = picture.clone();
    let media_select = media.clone();
    let playback_select = playback.clone();
    let window_select = window.clone();
    let entry_select = entry.clone();
    list.connect_row_selected(move |_, row| {
        if rebuilding.get() { return; }
        *armed_preview.borrow_mut() = None;
        if row.is_some() {
            let window = window_select.clone();
            let entry = entry_select.clone();
            glib::idle_add_local_once(move || {
                if window.is_visible() {
                    focus_search(&window, &entry);
                }
            });
        }
        let Some(row) = row else {
            *selected_preview.borrow_mut() = None;
            gen_preview.fetch_add(1, Ordering::Relaxed);
            stop_playback(&media_select, &playback_select);
            return;
        };
        let Some(result) = rows_preview.borrow().get(row.index() as usize).cloned() else {
            return;
        };
        let identity = result.identity();
        if selected_preview.borrow().as_ref() == Some(&identity) { return; }
        *selected_preview.borrow_mut() = Some(identity);
        let g = gen_preview.fetch_add(1, Ordering::Relaxed) + 1;
        preview_label.set_text("Loading preview…");
        picture_select.set_visible(false);
        stop_playback(&media_select, &playback_select);
        if let Action::File(path, _) = &result.action {
            let c = Paths::discover().load().unwrap_or_default();
            if c.preview && config::allowed(&c, path) && let Some(is_video) = media_kind(path)
                && let Some(player) = Player::new(path, is_video)
            {
                media_select.append(player.widget());
                *playback_select.borrow_mut() = Some(player);
            }
        }
        let query = query_entry.text().to_string();
        if utility::intent(&query) == utility::Intent::Calculation {
            return;
        }
        let tx = preview_tx.clone();
        std::thread::spawn(move || {
            let (text, img) = make_preview(&result, &query);
            let markup = match &result.action {
                Action::File(path, _) => code_markup(&text, path).unwrap_or_else(|| highlight(&text, &query)),
                _ => highlight(&text, &query),
            };
            let _ = tx.send((g, markup, img));
        });
    });
    let pic_poll = picture.clone();
    let win_weak = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(35), move || {
        if win_weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        while let Ok((g, markup, img)) = preview_rx.try_recv() {
            if g != preview_gen.load(Ordering::Relaxed) {
                continue;
            }
            preview.set_markup(&markup);
            pic_poll.set_visible(false);
            if let Some(bytes) = img
                && let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from(&bytes))
            {
                pic_poll.set_paintable(Some(&texture));
                pic_poll.set_visible(true);
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
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |_, _, _, _| browsing.set(true));
    list.add_controller(click);
    keys.connect_key_pressed(move |_, key, _, mods| {
        match key {
            gdk::Key::Escape => {
                win_keys.set_visible(false);
            }
            gdk::Key::Down | gdk::Key::Up => {
                let current = list_keys.selected_row().map(|r| r.index());
                let next = match (key == gdk::Key::Down, current) {
                    (true, Some(index)) => index + 1,
                    (false, Some(index)) => (index - 1).max(0),
                    (_, None) => 0,
                };
                if let Some(row) = list_keys.row_at_index(next) {
                    browsing_keys.set(true);
                    list_keys.select_row(Some(&row));
                    if let Some(bounds) = row.compute_bounds(&list_keys) {
                        let adj = scroll_keys.vadjustment();
                        let y = bounds.y() as f64;
                        let bottom = y + bounds.height() as f64;
                        if y < adj.value() {
                            adj.set_value(y.max(0.));
                        } else if bottom > adj.value() + adj.page_size() {
                            adj.set_value((bottom - adj.page_size()).min(adj.upper() - adj.page_size()).max(0.));
                        }
                    }
                }
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if let Some(row) = list_keys.selected_row()
                    && let Some(result) = rows_keys.borrow().get(row.index() as usize)
                {
                    activate(result, &win_keys, &footer_keys, &armed, mods.contains(gdk::ModifierType::CONTROL_MASK));
                }
            }
            gdk::Key::c if mods.contains(gdk::ModifierType::CONTROL_MASK) && !entry_keys.has_focus() => {
                if let Some(row) = list_keys.selected_row()
                    && let Some(result) = rows_keys.borrow().get(row.index() as usize)
                {
                    win_keys.clipboard().set_text(&result.title);
                }
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
    PANEL.with(|p| *p.borrow_mut() = Some(window.clone()));
    window.present();
    focus_search(&window, &entry);
    SEARCH.with(|s| *s.borrow_mut() = Some(entry.clone()));
    if let Some(query) = query {
        entry.set_text(query);
    }
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
fn make_preview(result: &ResultRow, _query: &str) -> (String, Option<Vec<u8>>) {
    let p = Paths::discover();
    let Ok(c) = p.load() else {
        return ("Invalid preferences".into(), None);
    };
    if !c.preview {
        return ("Previews disabled".into(), None);
    }
    match &result.action {
        Action::File(path, hit) => {
            if !config::allowed(&c, path) {
                return ("Source no longer permitted".into(), None);
            }
            let Ok(meta) = path.metadata() else {
                return ("File unavailable".into(), None);
            };
            let description = format!(
                "{}\n{} bytes\n{}",
                path.display(),
                meta.len(),
                if hit.is_null() { String::new() } else { format!("Match: {}", hit["info"]) }
            );
            let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
            if let Some(is_video) = media_kind(path) {
                if crate::player::available() {
                    return (description, None);
                }
                return media_preview(path, is_video, description);
            }
            if ext == "pdf" {
                let mut image_cmd = Command::new("pdftoppm");
                let page = hit["info"]["page"].as_u64().unwrap_or(1).max(1).to_string();
                image_cmd.args(["-f", &page, "-l", &page, "-singlefile", "-scale-to", "700", "-png"]).arg(path);
                if let Ok(bytes) = nixe_glimpse::process::capture(image_cmd, vec![], Duration::from_secs(3), 4 * 1024 * 1024) {
                    return (description, Some(bytes));
                }
                let mut cmd = Command::new("pdftotext");
                let page = hit["info"]["page"]
                    .as_u64()
                    .or_else(|| hit["loc"].as_f64().map(|n| n as u64))
                    .unwrap_or(1)
                    .max(1)
                    .to_string();
                cmd.args(["-f", &page, "-l", &page, "--"]).arg(path).arg("-");
                if let Ok(bytes) = nixe_glimpse::process::capture(cmd, vec![], Duration::from_secs(3), 65536) {
                    return (format!("{description}\n\n{}", String::from_utf8_lossy(&bytes)), None);
                }
            }
            if ["png", "jpg", "jpeg", "webp", "gif", "heic", "heif", "bmp", "tiff", "tif", "svg", "avif"].contains(&ext.as_str()) {
                let mut cmd = Command::new("ffmpeg");
                cmd.args(["-v", "error", "-threads", "1", "-i"]).arg(path).args([
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=512:512:force_original_aspect_ratio=decrease",
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "png",
                    "-",
                ]);
                if let Ok(bytes) = nixe_glimpse::process::capture(cmd, vec![], Duration::from_secs(3), 2 * 1024 * 1024) {
                    return (description, Some(bytes));
                }
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
                    let mut text = String::new();
                    let mut tag = false;
                    for ch in String::from_utf8_lossy(&bytes).chars() {
                        match ch {
                            '<' => { tag = true; text.push(' '); }
                            '>' => tag = false,
                            _ if !tag => text.push(ch),
                            _ => {}
                        }
                    }
                    return (format!("{description}\n\n{}", text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").chars().take(16000).collect::<String>()), None);
                }
            }
            if let Ok(f) = std::fs::File::open(path) {
                let mut bytes = Vec::new();
                if f.take(65536).read_to_end(&mut bytes).is_ok() && !bytes.contains(&0) {
                    return (format!("{description}\n\n{}", String::from_utf8_lossy(&bytes)), None);
                }
            }
            (format!("{description}\n\nNo inline preview is available for this file type. Press Enter to open it in its application."), None)
        }
        Action::Copy(_) => (format!("{}\n\n{}", result.title, result.detail), None),
        Action::Plugin(id, result_id, _) => {
            if let Ok(plugin) = plugins::get(&p, id)
                && plugin.manifest.capabilities.contains(&"preview".into())
                && let Ok(v) = plugins::call(
                    &p,
                    id,
                    "preview",
                    "preview",
                    json!({"result_id":result_id,"title":result.title,"query":_query}),
                )
            {
                return (v["text"].as_str().unwrap_or("No preview").chars().take(16000).collect(), None);
            }
            (result.detail.clone(), None)
        }
        _ => (result.detail.clone(), None),
    }
}
fn activate(
    result: &ResultRow,
    window: &gtk::ApplicationWindow,
    status: &gtk::Label,
    armed: &RefCell<Option<String>>,
    reveal: bool,
) {
    if let Action::Plugin(id, result_id, plugins::Action::Activate { token }) = &result.action {
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
                            detail: String::new(),
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
                    status.set_text("Press Enter again to confirm this system action.");
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
                let action =
                    if let plugins::Action::Activate { token } = action { plugins::activate(&p, &id, &token)? } else { action };
                match action {
                    plugins::Action::Copy { text } => window.clipboard().set_text(&text),
                    plugins::Action::Open { path } => open_file(&path, &Value::Null, reveal)?,
                    plugins::Action::Url { url } => gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)?,
                    plugins::Action::Activate { .. } => unreachable!(),
                }
            }
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
    let (content_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);
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
    text.split(keyword)
        .map(|s| glib::markup_escape_text(s).to_string())
        .collect::<Vec<_>>()
        .join(&format!("<b>{}</b>", glib::markup_escape_text(keyword)))
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
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            for (label, op) in [
                (if plugin.grant.enabled { "Disable" } else { "Enable · approve listed permissions" }, "toggle"),
                ("Update", "update"),
                ("Rollback", "rollback"),
                ("Remove", "remove"),
            ] {
                let button = gtk::Button::with_label(label);
                let id = plugin.manifest.id.clone();
                let enabled = plugin.grant.enabled;
                let st = status.clone();
                button.connect_clicked(move |_| {
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
                });
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

/// Without libmpv there is no inline player: show a still frame and the running time.
fn stop_playback(slot: &gtk::Box, playback: &RefCell<Option<Player>>) {
    if let Some(player) = playback.borrow_mut().take() {
        slot.remove(player.widget());
    }
}
fn media_preview(path: &Path, is_video: bool, description: String) -> (String, Option<Vec<u8>>) {
    let mut probe = Command::new("ffprobe");
    probe.args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"]).arg(path);
    let description = match nixe_glimpse::process::capture(probe, vec![], Duration::from_secs(2), 256)
        .ok()
        .and_then(|b| String::from_utf8_lossy(&b).trim().parse::<f64>().ok())
    {
        Some(secs) => {
            let secs = secs.round() as u64;
            let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
            let length = if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") };
            format!("{}\nLength: {length}", description.trim_end())
        }
        None => description,
    };
    if !is_video {
        return (description, None);
    }
    // Skip into the video past black intro frames; short clips fall back to the start.
    for seek in ["5", "0"] {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-v", "error", "-threads", "1", "-ss", seek, "-i"]).arg(path).args([
            "-frames:v",
            "1",
            "-vf",
            "scale=640:360:force_original_aspect_ratio=decrease",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ]);
        if let Ok(bytes) = nixe_glimpse::process::capture(cmd, vec![], Duration::from_secs(3), 4 * 1024 * 1024)
            && !bytes.is_empty()
        {
            return (description, Some(bytes));
        }
    }
    (description, None)
}
fn media_kind(path: &Path) -> Option<bool> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ["mp4", "mkv", "webm", "mov", "avi", "m4v", "mpeg", "mpg", "ogv", "ts"].contains(&ext.as_str()) {
        Some(true)
    } else if ["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac", "aiff", "wma"].contains(&ext.as_str()) {
        Some(false)
    } else { None }
}
fn code_markup(text: &str, path: &Path) -> Option<String> {
    use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings};
    thread_local! {
        static SYNTAXES: SyntaxSet = SyntaxSet::load_defaults_newlines();
        static THEMES: ThemeSet = ThemeSet::load_defaults();
    }
    let ext = path.extension()?.to_str()?;
    let (description, code) = text.split_once("\n\n")?;
    SYNTAXES.with(|ss| {
        let syntax = ss.find_syntax_by_extension(ext)?;
        THEMES.with(|themes| {
            let mut highlighter = HighlightLines::new(syntax, &themes.themes[if nixe_theme::current_palette().light { "InspiredGitHub" } else { "base16-ocean.dark" }]);
            let mut markup = format!("{}\n\n<span font_family=\"monospace\">", glib::markup_escape_text(description));
            for line in LinesWithEndings::from(code) {
                for (style, token) in highlighter.highlight_line(line, ss).ok()? {
                    let c = style.foreground;
                    markup.push_str(&format!("<span foreground=\"#{:02x}{:02x}{:02x}\">{}</span>", c.r, c.g, c.b, glib::markup_escape_text(token)));
                }
            }
            markup.push_str("</span>");
            Some(markup)
        })
    })
}
pub fn dismiss() {
    PANEL.with(|p| { if let Some(window) = p.borrow().as_ref() { window.set_visible(false); } });
}

fn navigate_result(list: &gtk::ListBox, direction: i32) {
    let next = list.selected_row().map(|row| (row.index() + direction).max(0)).unwrap_or(0);
    if let Some(row) = list.row_at_index(next) { list.select_row(Some(&row)); }
}
