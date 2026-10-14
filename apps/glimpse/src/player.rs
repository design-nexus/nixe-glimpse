//! Inline audio/video preview on libmpv, loaded at runtime. mpv renders frames in
//! software into a texture, so playback never touches GTK's GStreamer backend or the
//! window's GL context (GtkMediaFile stops the window repainting on the GL renderer).
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::Path,
    ptr,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

type Handle = *mut c_void;

#[repr(C)]
struct Param {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct Event {
    id: c_int,
    error: c_int,
    reply: u64,
    data: *mut c_void,
}

#[repr(C)]
struct EventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

struct Api {
    create: unsafe extern "C" fn() -> Handle,
    initialize: unsafe extern "C" fn(Handle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(Handle),
    set_option_string: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    set_property_string: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(Handle, *const c_char, c_int, *mut c_void) -> c_int,
    command: unsafe extern "C" fn(Handle, *mut *const c_char) -> c_int,
    observe_property: unsafe extern "C" fn(Handle, u64, *const c_char, c_int) -> c_int,
    wait_event: unsafe extern "C" fn(Handle, f64) -> *mut Event,
    render_create: unsafe extern "C" fn(*mut Handle, Handle, *mut Param) -> c_int,
    render_set_update_callback: unsafe extern "C" fn(Handle, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    render_update: unsafe extern "C" fn(Handle) -> u64,
    render: unsafe extern "C" fn(Handle, *mut Param) -> c_int,
    render_free: unsafe extern "C" fn(Handle),
}

const FORMAT_NONE: c_int = 0;
const FORMAT_FLAG: c_int = 3;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const EVENT_NONE: c_int = 0;
const EVENT_VIDEO_RECONFIG: c_int = 17;
const EVENT_PLAYBACK_RESTART: c_int = 21;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const PARAM_END: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
const PARAM_SW_SIZE: c_int = 17;
const PARAM_SW_FORMAT: c_int = 18;
const PARAM_SW_STRIDE: c_int = 19;
const PARAM_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;
const TIME_POS: u64 = 1;
const DURATION: u64 = 2;
const PAUSE: u64 = 3;
const EOF: u64 = 4;

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = libc::dlopen(c"libmpv.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal) => {{
                let p = libc::dlsym(lib, concat!($name, "\0").as_ptr().cast());
                if p.is_null() {
                    return None;
                }
                std::mem::transmute(p)
            }};
        }
        Some(Api {
            create: sym!("mpv_create"),
            initialize: sym!("mpv_initialize"),
            terminate_destroy: sym!("mpv_terminate_destroy"),
            set_option_string: sym!("mpv_set_option_string"),
            set_property_string: sym!("mpv_set_property_string"),
            get_property: sym!("mpv_get_property"),
            command: sym!("mpv_command"),
            observe_property: sym!("mpv_observe_property"),
            wait_event: sym!("mpv_wait_event"),
            render_create: sym!("mpv_render_context_create"),
            render_set_update_callback: sym!("mpv_render_context_set_update_callback"),
            render_update: sym!("mpv_render_context_update"),
            render: sym!("mpv_render_context_render"),
            render_free: sym!("mpv_render_context_free"),
        })
    })
    .as_ref()
}

/// Whether inline playback is possible (libmpv is installed).
pub fn available() -> bool {
    api().is_some()
}

unsafe extern "C" fn on_update(data: *mut c_void) {
    unsafe { (*(data as *const AtomicBool)).store(true, Ordering::Release) };
}

fn clock(secs: f64) -> String {
    let secs = secs.max(0.).round() as u64;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

struct State {
    api: &'static Api,
    mpv: Handle,
    render: Handle,
    dirty: Arc<AtomicBool>,
    picture: gtk::Picture,
    play: gtk::Button,
    scale: gtk::Scale,
    elapsed: gtk::Label,
    remaining: gtk::Label,
    duration: Cell<f64>,
    paused: Cell<bool>,
    eof: Cell<bool>,
    seeking: Cell<bool>,
    source: Cell<(i64, i64)>,
    frame: RefCell<Vec<u8>>,
}

impl State {
    fn command(&self, args: &[&str]) {
        let args: Vec<CString> = args.iter().filter_map(|a| CString::new(*a).ok()).collect();
        let mut ptrs: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(ptr::null());
        unsafe { (self.api.command)(self.mpv, ptrs.as_mut_ptr()) };
    }

    fn set(&self, name: &CStr, value: &str) {
        let Ok(value) = CString::new(value) else { return };
        unsafe { (self.api.set_property_string)(self.mpv, name.as_ptr(), value.as_ptr()) };
    }

    fn int(&self, name: &CStr) -> i64 {
        let mut value = 0i64;
        unsafe { (self.api.get_property)(self.mpv, name.as_ptr(), FORMAT_INT64, (&mut value as *mut i64).cast()) };
        value
    }

    fn toggle(&self) {
        if self.paused.get() && self.eof.get() {
            self.command(&["seek", "0", "absolute"]);
        }
        self.set(c"pause", if self.paused.get() { "no" } else { "yes" });
    }

    fn poll(&self) {
        loop {
            let event = unsafe { (self.api.wait_event)(self.mpv, 0.) };
            if event.is_null() {
                break;
            }
            let event = unsafe { &*event };
            match event.id {
                EVENT_NONE => break,
                EVENT_VIDEO_RECONFIG => self.source.set((self.int(c"dwidth"), self.int(c"dheight"))),
                EVENT_PLAYBACK_RESTART => self.seeking.set(false),
                EVENT_PROPERTY_CHANGE if !event.data.is_null() => {
                    let property = unsafe { &*(event.data as *const EventProperty) };
                    if property.format == FORMAT_NONE || property.data.is_null() {
                        continue;
                    }
                    match (event.reply, property.format) {
                        (TIME_POS, FORMAT_DOUBLE) => {
                            let at = unsafe { *(property.data as *const f64) };
                            if !self.seeking.get() {
                                self.scale.set_value(at);
                            }
                            self.elapsed.set_text(&clock(at));
                            self.remaining.set_text(&format!("-{}", clock(self.duration.get() - at)));
                        }
                        (DURATION, FORMAT_DOUBLE) => {
                            let duration = unsafe { *(property.data as *const f64) };
                            self.duration.set(duration);
                            self.scale.set_range(0., duration.max(0.01));
                            self.remaining.set_text(&format!("-{}", clock(duration - self.scale.value())));
                        }
                        (PAUSE, FORMAT_FLAG) => {
                            let paused = unsafe { *(property.data as *const c_int) } != 0;
                            self.paused.set(paused);
                            self.play.set_icon_name(if paused { "media-playback-start-symbolic" } else { "media-playback-pause-symbolic" });
                        }
                        (EOF, FORMAT_FLAG) => self.eof.set(unsafe { *(property.data as *const c_int) } != 0),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if self.dirty.swap(false, Ordering::AcqRel) && unsafe { (self.api.render_update)(self.render) } & UPDATE_FRAME != 0 {
            self.draw();
        }
    }

    fn draw(&self) {
        let (w, h) = self.source.get();
        if w <= 0 || h <= 0 || !self.picture.is_visible() {
            return;
        }
        // Render near the size shown, never above the source.
        let bound = match self.picture.width() * self.picture.scale_factor() {
            0 => 640.,
            px => (px as f64).clamp(320., 1280.),
        };
        let fit = (bound / w as f64).min(bound * 0.75 / h as f64).min(1.);
        let (tw, th) = (((w as f64 * fit) as i32).max(2), ((h as f64 * fit) as i32).max(2));
        let mut stride = (tw as usize * 4).div_ceil(64) * 64;
        let mut frame = self.frame.borrow_mut();
        frame.resize(stride * th as usize, 0);
        let mut size = [tw as c_int, th as c_int];
        // Rendering waits for each frame's display time by default, which would keep the
        // main loop busy and starve GTK's own repaints.
        let mut block: c_int = 0;
        let mut params = [
            Param { kind: PARAM_BLOCK_FOR_TARGET_TIME, data: (&mut block as *mut c_int).cast() },
            Param { kind: PARAM_SW_SIZE, data: size.as_mut_ptr().cast() },
            Param { kind: PARAM_SW_FORMAT, data: c"rgb0".as_ptr() as *mut c_void },
            Param { kind: PARAM_SW_STRIDE, data: (&mut stride as *mut usize).cast() },
            Param { kind: PARAM_SW_POINTER, data: frame.as_mut_ptr().cast() },
            Param { kind: PARAM_END, data: ptr::null_mut() },
        ];
        if unsafe { (self.api.render)(self.render, params.as_mut_ptr()) } < 0 {
            return;
        }
        let bytes = glib::Bytes::from(&frame[..]);
        let texture = gdk::MemoryTexture::new(tw, th, gdk::MemoryFormat::R8g8b8x8, &bytes, stride);
        self.picture.set_paintable(Some(&texture));
    }
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            (self.api.render_set_update_callback)(self.render, None, ptr::null_mut());
            (self.api.render_free)(self.render);
        }
        // Shutting the core down joins its threads; keep that off the UI thread.
        let (mpv, destroy) = (self.mpv as usize, self.api.terminate_destroy);
        std::thread::spawn(move || unsafe { destroy(mpv as Handle) });
    }
}

/// One loaded file, paused on its first frame. Dropping it stops playback.
pub struct Player {
    root: gtk::Box,
    state: Rc<State>,
}

impl Player {
    pub fn new(path: &Path, is_video: bool) -> Option<Player> {
        let api = api()?;
        // libmpv refuses to start under a non-C numeric locale.
        unsafe { libc::setlocale(libc::LC_NUMERIC, c"C".as_ptr()) };
        let mpv = unsafe { (api.create)() };
        if mpv.is_null() {
            return None;
        }
        let options: &[(&CStr, &CStr)] = &[
            (c"config", c"no"),
            (c"terminal", c"no"),
            (c"load-scripts", c"no"),
            (c"ytdl", c"no"),
            (c"input-default-bindings", c"no"),
            (c"osc", c"no"),
            (c"vo", c"libmpv"),
            (c"hwdec", c"no"),
            (c"keep-open", c"yes"),
            (c"pause", c"yes"),
            (c"vid", if is_video { c"auto" } else { c"no" }),
        ];
        for (name, value) in options {
            unsafe { (api.set_option_string)(mpv, name.as_ptr(), value.as_ptr()) };
        }
        let mut render = ptr::null_mut();
        let mut params = [
            Param { kind: PARAM_API_TYPE, data: c"sw".as_ptr() as *mut c_void },
            Param { kind: PARAM_END, data: ptr::null_mut() },
        ];
        if unsafe { (api.initialize)(mpv) } < 0 || unsafe { (api.render_create)(&mut render, mpv, params.as_mut_ptr()) } < 0 {
            unsafe { (api.terminate_destroy)(mpv) };
            return None;
        }

        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        // Small enough to fit the preview pane; it grows to fill the frame.
        picture.set_size_request(160, 90);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_visible(is_video);
        let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play.add_css_class("flat");
        play.set_focusable(false);
        play.set_focus_on_click(false);
        let elapsed = gtk::Label::new(Some("0:00"));
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0., 1., 1.);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_focusable(false);
        scale.set_focus_on_click(false);
        let remaining = gtk::Label::new(Some("-0:00"));
        let volume = gtk::ScaleButton::new(
            0.,
            100.,
            5.,
            &["audio-volume-muted-symbolic", "audio-volume-high-symbolic", "audio-volume-low-symbolic", "audio-volume-medium-symbolic"],
        );
        volume.set_value(100.);
        volume.set_focusable(false);
        volume.set_focus_on_click(false);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.add_css_class("player-controls");
        controls.append(&play);
        controls.append(&elapsed);
        controls.append(&scale);
        controls.append(&remaining);
        controls.append(&volume);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_vexpand(is_video);
        root.append(&picture);
        root.append(&controls);

        let state = Rc::new(State {
            api,
            mpv,
            render,
            dirty: Arc::new(AtomicBool::new(false)),
            picture,
            play: play.clone(),
            scale: scale.clone(),
            elapsed,
            remaining,
            duration: Cell::new(0.),
            paused: Cell::new(true),
            eof: Cell::new(false),
            seeking: Cell::new(false),
            source: Cell::new((0, 0)),
            frame: RefCell::new(Vec::new()),
        });
        unsafe {
            (api.render_set_update_callback)(render, Some(on_update), Arc::as_ptr(&state.dirty) as *mut c_void);
            for (id, name, format) in [
                (TIME_POS, c"time-pos", FORMAT_DOUBLE),
                (DURATION, c"duration", FORMAT_DOUBLE),
                (PAUSE, c"pause", FORMAT_FLAG),
                (EOF, c"eof-reached", FORMAT_FLAG),
            ] {
                (api.observe_property)(mpv, id, name.as_ptr(), format);
            }
        }
        state.command(&["loadfile", &path.to_string_lossy()]);

        let weak = Rc::downgrade(&state);
        play.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                state.toggle();
            }
        });
        let weak = Rc::downgrade(&state);
        scale.connect_change_value(move |_, _, value| {
            if let Some(state) = weak.upgrade() {
                state.seeking.set(true);
                state.command(&["seek", &format!("{value:.3}"), "absolute"]);
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(&state);
        volume.connect_value_changed(move |_, value| {
            if let Some(state) = weak.upgrade() {
                state.set(c"volume", &format!("{value:.0}"));
            }
        });
        let weak = Rc::downgrade(&state);
        // Below GTK's redraw priority, so the window always gets to paint between frames.
        glib::timeout_add_local_full(Duration::from_millis(15), glib::Priority::DEFAULT_IDLE, move || match weak.upgrade() {
            Some(state) => {
                state.poll();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        Some(Player { root, state })
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Play or pause; at the end of the file, play again from the start.
    pub fn toggle(&self) {
        self.state.toggle();
    }
}
