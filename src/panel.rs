// Панель настроек мыши: вытекает из иконки в трее «джинном», как окна на маке, и всасывается обратно.
// Layered-окно (попиксельная прозрачность): кадр рисуется GDI+ в 32-битный DIB и отдаётся UpdateLayeredWindow —
// отсюда сглаженные углы, мягкая тень и плавное появление. Вся работа с мышью — в фоновом потоке, чтобы не дёргалась анимация.
use crate::mouse::{self, Dev, Settings};
use crate::w;
use std::{
    cell::RefCell,
    mem, ptr,
    sync::{atomic::{AtomicU64, Ordering}, Mutex},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmFlush, Gdi::*, GdiPlus::*},
    System::{LibraryLoader::GetModuleHandleW, Threading::{AttachThreadInput, GetCurrentThreadId}},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

// ───── размеры (логические пиксели, умножаются на масштаб экрана) ─────
const W: f32 = 360.0;
const H: f32 = 498.0;
const PAD: f32 = 20.0;
const S: f32 = 16.0; // поле вокруг панели под тень
const GAP: f32 = 12.0; // отступ от панели задач
const OPEN_MS: f32 = 420.0;
const CLOSE_MS: f32 = 330.0;
const SLIDE: f32 = 40.0; // запасная анимация, если панель задач сверху или сбоку

// ───── цвета ARGB, как у трекера экранного времени ─────
const BG: u32 = 0xFF14_1A26;
const CARD: u32 = 0xFF20_293A;
const CARD_HOVER: u32 = 0xFF2E_3A50;
const FG: u32 = 0xFFEC_F0F6;
const MUTED: u32 = 0xFF8A_96AA;
const ACCENT: u32 = 0xFF4F_C3B0;
const GREEN: u32 = 0xFF34_C759;
const RED: u32 = 0xFFFF_3B30;
const YELLOW: u32 = 0xFFFF_D60A;

const WM_MOUSELEAVE: u32 = 0x02A3; // нет в windows-sys без лишней фичи
const PARGB: i32 = 0xE200B; // PixelFormat32bppPARGB
const WM_LOADED: u32 = WM_APP + 10;
const WM_CLOSE_PANEL: u32 = WM_APP + 11;
const WM_ANIM: u32 = WM_APP + 12;
const T_WATCH: usize = 2;
const T_DPI: usize = 3; // запись DPI после прокрутки колёсика — с задержкой, одной командой
const T_LEAVE: usize = 4; // курсор ушёл с панели — сворачиваем, если не вернулся
const T_RESET: usize = 5; // «нажми ещё раз» на кнопке сброса гаснет само

#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Stage(usize),
    Poll(u16),
    Lod(u8),
    Sleep(u16),
    Motion,
    Angle,
    Reset,
    Profile(u8), // сделать профиль активным (кнопка «›»)
    Name,        // имя профиля: двойной клик — переименовать
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Loading,
    Ready,
    NoMouse,
}

struct Panel {
    hwnd: HWND,
    scale: f32,
    // данные мыши
    set: Option<Settings>,
    battery: Option<(u8, bool)>,
    wired: bool,
    status: Status,
    // интерфейс
    hits: Vec<(RectF, Act)>,
    hover: Option<Act>,
    entered: bool, // курсор уже побывал на панели — только тогда уход с неё сворачивает
    reset_armed: bool,
    names: [String; 3],                  // свои имена профилей, хранятся на компе
    editing: Option<(usize, String)>,
    cache: [Option<Settings>; 3],        // настройки всех профилей: переключение показывается сразу, мышь догоняет в фоне    // переименовываем профиль: номер с нуля и набранный текст
    // анимация и место
    visible: bool,
    closing: bool,
    progress: f32,
    anim_from: f32,
    anim_start: Instant,
    hidden_at: Option<Instant>,
    anchor: POINT,
    panel_rc: RECT, // окно панели с полем под тень
    genie: bool,
    slide: (f32, f32),
    snap: Vec<u32>, // снимок панели для «джинна» (PARGB, ширина и высота — как у panel_rc); пусто — нет анимации
    snap_w: usize,
    fg_at_open: HWND,
    // буфер кадра
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    buf_w: i32,
    buf_h: i32,
    fonts: Fonts,
}

struct Fonts {
    regular: *mut GpFontFamily,
    semibold: *mut GpFontFamily,
}

thread_local! {
    static LOOPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static P: RefCell<Option<Panel>> = const { RefCell::new(None) };
}

// ───── результат фонового потока ─────
struct Loaded {
    seq: u64,
    set: Option<Settings>,
    battery: Option<(u8, bool)>,
    wired: bool,
    others: Vec<Settings>, // остальные профили — читаются при открытии панели
}
static RESULT: Mutex<Option<Loaded>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(0);
static DEV_PATH: Mutex<Option<Vec<u16>>> = Mutex::new(None);

type Job = Box<dyn FnOnce(&Dev) + Send>;

// Выполнить запись (если есть) и перечитать всё с мыши; ответ придёт окну сообщением WM_LOADED.
// Устаревшие ответы (пока шёл этот, начался новый) окно выбросит по номеру.
fn run(hwnd: HWND, job: Option<Job>) {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let hwnd = hwnd as isize;
    thread::spawn(move || {
        let _l = mouse::lock();
        let mut out = Loaded { seq, set: None, battery: None, wired: false, others: Vec::new() };
        let mut paths = crate::hid_paths();
        if let Some(p) = DEV_PATH.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            paths.insert(0, p);
        }
        let mut job = job;
        if std::env::var_os("R5_FAKE").is_some() {
            // только для скриншотов без мыши: подставные настройки
            paths.clear();
            out.set = Some(Settings { profile: 1, stages: vec![1200, 2400, 3200, 5600, 8000, 42000], active: 1, poll: 1000, lod: 1, sleep_s: 60, motion_sync: true, angle_snap: false });
            out.battery = Some((76, false));
        }
        for p in paths {
            let Some(d) = Dev::open(&p) else { continue };
            // быстрая проверка, что на этом интерфейсе мышь отвечает, — без полного чтения перед записью
            if d.cmd(1, 0, 0x85, &[]).is_none() {
                continue;
            }
            let opening = job.is_none();
            if let Some(j) = job.take() {
                j(&d);
            }
            out.set = d.read();
            if let (true, Some(cur)) = (opening, &out.set) {
                // после переключения мышь ~0,35 с подгружает профиль и молчит — поэтому все профили читаем заранее
                out.others = (1..=mouse::PROFILES).filter(|&n| n != cur.profile).filter_map(|n| d.read_profile(n)).collect();
            }
            out.battery = d.battery();
            out.wired = String::from_utf16_lossy(&p).to_ascii_lowercase().contains("pid_0046");
            *DEV_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
            break;
        }
        *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
        unsafe { PostMessageW(hwnd as HWND, WM_LOADED, 0, 0) };
    });
}

// ───── создание ─────

pub fn init() {
    unsafe {
        let mut token = 0usize;
        let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 };
        GdiplusStartup(&mut token, &input, ptr::null_mut());

        let hinst = GetModuleHandleW(ptr::null());
        let cls = w("R5Panel");
        let mut wc: WNDCLASSW = mem::zeroed();
        wc.lpfnWndProc = Some(proc_);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        wc.style = CS_DBLCLKS;
        wc.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, cls.as_ptr(), w("R5 Ultra").as_ptr(),
            WS_POPUP, 0, 0, 1, 1, ptr::null_mut(), ptr::null_mut(), hinst, ptr::null());

        let sdc = GetDC(ptr::null_mut());
        let scale = GetDeviceCaps(sdc, LOGPIXELSX as _) as f32 / 96.0;
        ReleaseDC(ptr::null_mut(), sdc);

        let mut regular = ptr::null_mut();
        let mut semibold = ptr::null_mut();
        GdipCreateFontFamilyFromName(w("Segoe UI").as_ptr(), ptr::null_mut(), &mut regular);
        GdipCreateFontFamilyFromName(w("Segoe UI Semibold").as_ptr(), ptr::null_mut(), &mut semibold);
        if semibold.is_null() {
            semibold = regular;
        }

        let p = Panel {
            hwnd, scale, set: None, battery: None, wired: false, status: Status::Loading,
            hits: Vec::new(), hover: None, entered: false, reset_armed: false, names: load_names(), editing: None, cache: [None, None, None], visible: false, closing: false, progress: 0.0, anim_from: 0.0,
            anim_start: Instant::now(), hidden_at: None, anchor: POINT { x: 0, y: 0 }, panel_rc: mem::zeroed(),
            genie: true, slide: (0.0, 1.0), snap: Vec::new(), snap_w: 0, fg_at_open: ptr::null_mut(),
            dc: ptr::null_mut(), dib: ptr::null_mut(), old: ptr::null_mut(), bits: ptr::null_mut(), buf_w: 0, buf_h: 0,
            fonts: Fonts { regular, semibold },
        };
        P.with(|c| *c.borrow_mut() = Some(p));
    }
}

fn with<R>(f: impl FnOnce(&mut Panel) -> R) -> Option<R> {
    P.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

// ───── открыть / закрыть ─────

// Клик по иконке: открыть или спрятать
// Спрятать сразу, без анимации: перед меню трея (его модальный цикл остановил бы кадры)
pub fn hide_now() {
    with(|p| {
        if !p.visible {
            return;
        }
        p.flush_dpi();
        p.finish_edit(true);
        p.drop_snap();
        p.visible = false;
        p.closing = false;
        p.progress = 0.0;
        p.hidden_at = Some(Instant::now());
        unsafe {
            KillTimer(p.hwnd, T_WATCH);
            KillTimer(p.hwnd, T_LEAVE);
            ShowWindow(p.hwnd, SW_HIDE);
        }
        p.move_to(p.panel_rc);
    });
}

pub fn toggle() {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut pt) };
    with(|p| {
        if p.visible && !p.closing {
            p.close();
        } else if p.hidden_at.is_some_and(|t| t.elapsed() < Duration::from_millis(400)) {
            // этот же клик по иконке только что увёл фокус и закрыл панель — не открываем снова
        } else {
            p.open(pt);
        }
    });
}

impl Panel {
    fn open(&mut self, anchor: POINT) {
        let was_open = self.visible && !self.closing;
        self.anchor = anchor;
        self.place();
        self.closing = false;
        self.hover = None;
        self.entered = false;
        self.reset_armed = false;
        if self.status != Status::Ready {
            self.status = Status::Loading;
        }
        run(self.hwnd, None);
        if was_open {
            return;
        }
        if self.genie {
            self.take_snap();
            let u = self.genie_rect();
            self.move_to(u);
        } else {
            self.move_to(self.panel_rc);
        }
        if !self.visible {
            self.progress = 0.0;
            self.visible = true;
            self.render();
            unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        }
        // открыли из окна скрытых значков — фокус не забираем, иначе Windows его закроет
        if overflow_rect().is_none() {
            force_foreground(self.hwnd);
        }
        self.fg_at_open = unsafe { GetForegroundWindow() };
        unsafe { SetTimer(self.hwnd, T_WATCH, 150, None) };
        self.start_anim();
    }

    fn close(&mut self) {
        if !self.visible || self.closing {
            return;
        }
        self.flush_dpi();
        self.finish_edit(true);
        self.closing = true;
        self.hidden_at = Some(Instant::now());
        self.hover = None;
        if self.genie {
            if self.snap.is_empty() {
                self.take_snap();
            }
            let u = self.genie_rect();
            self.move_to(u);
        }
        self.start_anim();
    }

    fn start_anim(&mut self) {
        self.anim_from = self.progress;
        self.anim_start = Instant::now();
        if !LOOPING.get() {
            unsafe { PostMessageW(self.hwnd, WM_ANIM, 0, 0) };
        }
    }

    // Один кадр; false — анимация закончилась
    fn animate(&mut self) -> bool {
        if !self.visible {
            return false; // спрятали мгновенно (hide_now) — кадры больше не нужны
        }
        let ms = if self.genie { if self.closing { CLOSE_MS } else { OPEN_MS } } else if self.closing { 160.0 } else { 230.0 };
        let t = (self.anim_start.elapsed().as_secs_f32() * 1000.0 / ms).min(1.0);
        self.progress = if self.closing { self.anim_from * (1.0 - t) } else { self.anim_from + (1.0 - self.anim_from) * t };
        if t >= 1.0 {
            self.drop_snap();
            if self.closing {
                self.closing = false;
                self.visible = false;
                unsafe {
                    KillTimer(self.hwnd, T_WATCH);
                    ShowWindow(self.hwnd, SW_HIDE);
                }
                self.move_to(self.panel_rc);
                return false; // спрятанную панель не рисуем
            }
            self.move_to(self.panel_rc); // окно снова ровно по панели — чтобы мышь попадала куда надо
        }
        self.render();
        t < 1.0
    }

    fn animating(&self) -> bool {
        self.closing || self.progress < 1.0
    }

    // Панель у панели задач над иконкой; панель задач может быть снизу, сверху или сбоку
    fn place(&mut self) {
        let a = self.anchor;
        let (wa, b) = monitor_rects(a);
        let s = self.scale;
        let (pw, ph, gap, sh) = ((W * s) as i32, (H * s) as i32, (GAP * s) as i32, (S * s) as i32);
        let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
        let (x, mut y);
        if wa.top > b.top {
            self.slide = (0.0, -1.0);
            y = wa.top + gap;
            x = clamp(a.x - pw / 2, wa.left + gap, wa.right - gap - pw);
        } else if wa.left > b.left {
            self.slide = (-1.0, 0.0);
            x = wa.left + gap;
            y = clamp(a.y - ph / 2, wa.top + gap, wa.bottom - gap - ph);
        } else if wa.right < b.right {
            self.slide = (1.0, 0.0);
            x = wa.right - gap - pw;
            y = clamp(a.y - ph / 2, wa.top + gap, wa.bottom - gap - ph);
        } else {
            self.slide = (0.0, 1.0);
            y = wa.bottom - gap - ph;
            x = clamp(a.x - pw / 2, wa.left + gap, wa.right - gap - pw);
            // открыто окно скрытых значков — встаём над ним, чтобы не закрывать трей
            if let Some(ov) = overflow_rect() {
                if ov.right > x && ov.left < x + pw {
                    y = y.min(ov.top - gap - ph).max(wa.top + gap);
                }
            }
        }
        // shortcut: «джинн» только для панели задач снизу (так почти у всех); сверху и сбоку — простой выезд
        self.genie = self.slide == (0.0, 1.0);
        self.panel_rc = RECT { left: x - sh, top: y - sh, right: x + pw + sh, bottom: y + ph + sh };
    }

    fn genie_rect(&self) -> RECT {
        let r = self.panel_rc;
        let a = self.anchor;
        RECT { left: r.left.min(a.x - 8), top: r.top.min(a.y - 8), right: r.right.max(a.x + 8), bottom: r.bottom.max(a.y + 8) }
    }

    fn move_to(&mut self, r: RECT) {
        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top,
                SWP_NOACTIVATE | SWP_NOREDRAW);
        }
    }

    fn window_rect(&self) -> RECT {
        let mut r: RECT = unsafe { mem::zeroed() };
        unsafe { GetWindowRect(self.hwnd, &mut r) };
        r
    }

    // ───── мышь и клавиатура ─────

    fn logical(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp & 0xFFFF) as i16 as f32;
        let y = ((lp >> 16) & 0xFFFF) as i16 as f32;
        (x / self.scale - S, y / self.scale - S)
    }

    fn hit(&self, (x, y): (f32, f32)) -> Option<Act> {
        self.hits.iter().find(|(r, _)| x >= r.X && x < r.X + r.Width && y >= r.Y && y < r.Y + r.Height).map(|h| h.1)
    }

    fn mouse_move(&mut self, lp: LPARAM) {
        if self.animating() {
            return;
        }
        let h = self.hit(self.logical(lp));
        self.entered = true;
        unsafe {
            KillTimer(self.hwnd, T_LEAVE);
            SetCursor(LoadCursorW(ptr::null_mut(), if h.is_some() { IDC_HAND } else { IDC_ARROW }));
            let mut tme = TRACKMOUSEEVENT { cbSize: mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
        }
        if h != self.hover {
            self.hover = h;
            self.render();
        }
    }

    fn click(&mut self, lp: LPARAM) {
        if self.animating() {
            return;
        }
        let act = self.hit(self.logical(lp));
        if act == Some(Act::Name) {
            return; // имя меняется двойным кликом, одиночный ничего не делает
        }
        if self.editing.is_some() {
            self.finish_edit(true);
            self.render();
        }
        let Some(act) = act else { return };
        if let Act::Profile(n) = act {
            self.switch_profile(n);
            return;
        }
        if act == Act::Reset && !self.reset_armed {
            // сброс — только со второго клика, чтобы не стереть настройки случайно
            self.reset_armed = true;
            unsafe { SetTimer(self.hwnd, T_RESET, 3000, None) };
            self.render();
            return;
        }
        let Some(s) = self.set.as_mut() else { return };
        let p = s.profile;

        // меняем на экране сразу, запись — в фоне; после записи мышь перечитывается и покажет правду
        let job: Job = match act {
            Act::Stage(i) => {
                s.active = i;
                Box::new(move |d| { d.set_active(p, i); })
            }
            Act::Poll(hz) => {
                s.poll = hz;
                Box::new(move |d| { d.set_poll(p, hz); })
            }
            Act::Lod(c) => {
                s.lod = c;
                Box::new(move |d| { d.set_lod(p, c); })
            }
            Act::Sleep(sec) => {
                s.sleep_s = sec;
                Box::new(move |d| { d.set_sleep(p, sec); })
            }
            Act::Motion => {
                s.motion_sync = !s.motion_sync;
                let on = s.motion_sync;
                Box::new(move |d| { d.set_motion_sync(p, on); })
            }
            Act::Profile(_) | Act::Name => return,
            Act::Reset => {
                self.reset_armed = false;
                unsafe { KillTimer(self.hwnd, T_RESET) };
                unsafe { KillTimer(self.hwnd, T_DPI) }; // недописанные DPI после сброса не нужны
                Box::new(move |d| { d.reset_profile(p); })
            }
            Act::Angle => {
                s.angle_snap = !s.angle_snap;
                let on = s.angle_snap;
                Box::new(move |d| { d.set_angle_snap(p, on); })
            }
        };
        self.flush_dpi();
        run(self.hwnd, Some(job));
        self.render();
    }

    // Колёсико над ступенью DPI — меняет её значение; запишется через полсекунды после последней прокрутки
    fn wheel(&mut self, wp: WPARAM) {
        let Some(Act::Stage(i)) = self.hover else { return };
        let Some(s) = self.set.as_mut() else { return };
        let up = ((wp >> 16) & 0xFFFF) as i16 > 0;
        let v = s.stages[i] as i32;
        let step = if v < 2000 { 50 } else if v < 10000 { 100 } else { 1000 };
        s.stages[i] = mouse::clamp_dpi(if up { v + step } else { v - step });
        unsafe { SetTimer(self.hwnd, T_DPI, 500, None) };
        self.render();
    }

    // Сразу показываем настройки профиля из запаса, а мышь переключается в фоне
    fn switch_profile(&mut self, n: u8) {
        let Some(cur) = self.set.clone() else { return };
        if cur.profile == n {
            return;
        }
        self.flush_dpi(); // недописанные DPI — в старый профиль
        self.cache[(cur.profile as usize).clamp(1, 3) - 1] = Some(cur.clone());
        self.set = Some(match self.cache[n as usize - 1].clone() {
            Some(c) => c,
            None => Settings { profile: n, ..cur },
        });
        run(self.hwnd, Some(Box::new(move |d| { d.set_profile(n); })));
        self.render();
    }

    // Двойной клик по имени профиля — ввод нового имени прямо на месте
    fn dblclick(&mut self, lp: LPARAM) {
        if let (Some(Act::Name), Some(s)) = (self.hit(self.logical(lp)), &self.set) {
            let i = (s.profile as usize).clamp(1, 3) - 1;
            self.editing = Some((i, self.names[i].clone()));
            force_foreground(self.hwnd); // иначе буквы уйдут в другое окно
            self.render();
        }
    }

    // Набор имени: буквы, Backspace, Enter — сохранить, Esc — отменить
    fn char_in(&mut self, c: u32) -> bool {
        let Some((_, buf)) = self.editing.as_mut() else { return false };
        match c {
            8 => { buf.pop(); }
            13 => self.finish_edit(true),
            27 => self.finish_edit(false),
            c if c >= 32 && buf.chars().count() < 16 => buf.extend(char::from_u32(c)),
            _ => {}
        }
        self.render();
        true
    }

    fn finish_edit(&mut self, save: bool) {
        if let Some((i, buf)) = self.editing.take() {
            if save {
                self.names[i] = buf.trim().to_string();
                save_names(&self.names);
            }
        }
    }

    fn flush_dpi(&mut self) {
        unsafe {
            if KillTimer(self.hwnd, T_DPI) == 0 {
                return; // таймера не было — записывать нечего
            }
        }
        if let Some(s) = &self.set {
            let (p, stages) = (s.profile, s.stages.clone());
            run(self.hwnd, Some(Box::new(move |d| { d.set_stages(p, &stages); })));
        }
    }

    fn loaded(&mut self) {
        let Some(r) = RESULT.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        if r.seq != SEQ.load(Ordering::SeqCst) {
            return; // пока читали, пользователь уже кликнул ещё — ждём свежий ответ
        }
        if unsafe { KillTimer(self.hwnd, T_DPI) } != 0 {
            unsafe { SetTimer(self.hwnd, T_DPI, 500, None) }; // крутят колёсико — не затираем их цифры
            return;
        }
        self.status = if r.set.is_some() { Status::Ready } else { Status::NoMouse };
        for s in r.others.into_iter().chain(r.set.clone()) {
            let i = (s.profile as usize).clamp(1, 3) - 1;
            self.cache[i] = Some(s);
        }
        if r.set.is_some() {
            self.set = r.set;
            self.wired = r.wired;
        }
        if r.battery.is_some() {
            self.battery = r.battery;
        }
        if !self.snap.is_empty() {
            self.take_snap(); // данные пришли посреди анимации — следующий кадр уже с ними, без скачка в конце
        } else if self.visible && !self.animating() {
            self.render();
        }
    }

    // ───── отрисовка ─────

    fn ensure_buffer(&mut self, w: i32, h: i32) {
        if !self.dc.is_null() && w == self.buf_w && h == self.buf_h {
            return;
        }
        unsafe {
            if !self.dc.is_null() {
                SelectObject(self.dc, self.old);
                DeleteObject(self.dib);
                DeleteDC(self.dc);
            }
            let mut bi: BITMAPINFO = mem::zeroed();
            bi.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            self.dc = CreateCompatibleDC(ptr::null_mut());
            let mut bits = ptr::null_mut();
            self.dib = CreateDIBSection(self.dc, &bi, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
            self.old = SelectObject(self.dc, self.dib);
            self.bits = bits as *mut u8;
            self.buf_w = w;
            self.buf_h = h;
        }
    }

    fn render(&mut self) {
        let r = self.window_rect();
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return;
        }
        self.ensure_buffer(w, h);
        unsafe {
            let mut bmp = ptr::null_mut();
            GdipCreateBitmapFromScan0(w, h, w * 4, PARGB, self.bits, &mut bmp);
            let mut g = ptr::null_mut();
            GdipGetImageGraphicsContext(bmp as *mut GpImage, &mut g);
            GdipGraphicsClear(g, 0);
            let alpha;
            if !self.snap.is_empty() {
                let pr = self.panel_rc;
                let dst = std::slice::from_raw_parts_mut(self.bits as *mut u32, (w * h) as usize);
                alpha = genie_frame(dst, w as usize, &self.snap, self.snap_w,
                    ((pr.left - r.left) as f32, (pr.top - r.top) as f32),
                    ((self.anchor.x - r.left) as f32, (self.anchor.y - r.top) as f32), self.progress);
            } else {
                let e = 1.0 - (1.0 - self.progress).powi(3);
                let (dx, dy) = (self.slide.0 * SLIDE * (1.0 - e), self.slide.1 * SLIDE * (1.0 - e));
                GdipScaleWorldTransform(g, self.scale, self.scale, MatrixOrderPrepend);
                GdipTranslateWorldTransform(g, S + dx, S + dy, MatrixOrderPrepend);
                self.draw_all(g);
                alpha = (255.0 * e) as u8;
            }
            GdipDeleteGraphics(g);
            GdipDisposeImage(bmp as *mut GpImage);

            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: 1 };
            UpdateLayeredWindow(self.hwnd, ptr::null_mut(), ptr::null(), &size, self.dc, &src, 0, &blend, ULW_ALPHA);
        }
    }

    // Снимок панели целиком: «джинн» гнёт его построчно
    fn take_snap(&mut self) {
        let r = self.panel_rc;
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut buf = vec![0u32; (w * h) as usize];
        unsafe {
            let mut bmp = ptr::null_mut();
            GdipCreateBitmapFromScan0(w, h, w * 4, PARGB, buf.as_mut_ptr() as *mut u8, &mut bmp);
            let mut g = ptr::null_mut();
            GdipGetImageGraphicsContext(bmp as *mut GpImage, &mut g);
            GdipScaleWorldTransform(g, self.scale, self.scale, MatrixOrderPrepend);
            GdipTranslateWorldTransform(g, S, S, MatrixOrderPrepend);
            self.draw_all(g);
            GdipDeleteGraphics(g);
            GdipDisposeImage(bmp as *mut GpImage);
        }
        self.snap = buf;
        self.snap_w = w as usize;
    }

    fn drop_snap(&mut self) {
        self.snap = Vec::new();
    }

    unsafe fn draw_all(&mut self, g: *mut GpGraphics) {
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        // ClearType на прозрачном окне даёт цветную бахрому — только сглаживание в оттенках серого
        GdipSetTextRenderingHint(g, TextRenderingHintAntiAliasGridFit);
        let d = Draw { g, f: &self.fonts };
        let mut hits = Vec::new();

        // карточка с тенью
        for i in (1..=S as i32).rev().step_by(2) {
            let i = i as f32;
            d.round(-i + 2.0, -i + 5.0, W + 2.0 * i - 4.0, H + 2.0 * i - 4.0, 14.0 + i, 0x0900_0000);
        }
        d.round(0.0, 0.0, W, H, 14.0, BG);
        d.round_outline(0.0, 0.0, W, H, 14.0, 0x1CFF_FFFF);

        // шапка: название и заряд
        d.text("R5 Ultra", 18.0, true, FG, rf(PAD, 16.0, 200.0, 26.0), Align::Left);
        if let Some((pct, ch)) = self.battery {
            let x = W - PAD - 30.0;
            let fill = if ch { YELLOW } else if pct <= 20 { RED } else { GREEN };
            d.round_outline(x, 22.0, 26.0, 13.0, 3.5, MUTED);
            d.round(x + 26.5, 25.5, 2.5, 6.0, 1.0, MUTED);
            d.round(x + 2.0, 24.0, 22.0 * pct.min(100) as f32 / 100.0, 9.0, 2.0, fill);
            let label = if ch { format!("{pct}% · заряжается") } else { format!("{pct}%") };
            d.text(&label, 13.0, true, FG, rf(x - 190.0, 19.0, 184.0, 20.0), Align::Right);
        }
        let hov = self.hover;
        let status = match self.status {
            Status::Loading if self.set.is_none() => Some("Читаю настройки мыши…"),
            Status::NoMouse => Some("Мышь спит или не подключена — пошевели её"),
            _ => None,
        };
        if let Some(t) = status {
            d.text(t, 12.0, false, MUTED, rf(PAD, 44.0, W - 2.0 * PAD, 18.0), Align::Left);
        }

        let Some(s) = self.set.clone() else {
            self.hits = hits;
            return;
        };
        if status.is_none() {
            // имя профиля (двойной клик — переименовать), «›» — следующий профиль, способ подключения
            let i = (s.profile as usize).clamp(1, 3) - 1;
            let name = match &self.editing {
                Some((_, buf)) => format!("{buf}|"),
                None if self.names[i].is_empty() => format!("Профиль {}", s.profile),
                None => self.names[i].clone(),
            };
            let nw = d.measure(&name, 12.0, true).max(12.0);
            if self.editing.is_some() {
                d.round(PAD - 5.0, 41.0, nw + 10.0, 23.0, 7.0, CARD_HOVER);
            }
            d.text(&name, 12.0, true, FG, rf(PAD, 44.0, nw + 4.0, 18.0), Align::Left);
            hits.push((rf(PAD - 5.0, 40.0, nw + 10.0, 25.0), Act::Name));
            let next = s.profile % mouse::PROFILES + 1;
            let b = rf(PAD + nw + 6.0, 41.0, 22.0, 22.0);
            d.round(b.X, b.Y, b.Width, b.Height, 7.0, if hov == Some(Act::Profile(next)) { CARD_HOVER } else { CARD });
            d.text("›", 15.0, true, FG, rf(b.X, b.Y - 1.5, b.Width, b.Height), Align::Center);
            hits.push((b, Act::Profile(next)));
            let conn = if self.wired { "· по проводу" } else { "· через донгл" };
            d.text(conn, 12.0, false, MUTED, rf(b.X + b.Width + 6.0, 44.0, 200.0, 18.0), Align::Left);
        }

        // DPI: большая цифра активной ступени и сами ступени
        d.text("DPI", 12.0, false, MUTED, rf(PAD, 76.0, 100.0, 18.0), Align::Left);
        d.text(&s.stages[s.active].to_string(), 34.0, true, FG, rf(PAD - 2.0, 92.0, 200.0, 44.0), Align::Left);
        d.text("клик — выбрать
колёсико — изменить", 11.0, false, MUTED, rf(W - PAD - 160.0, 100.0, 160.0, 34.0), Align::Right);
        let n = s.stages.len() as f32;
        let cw = (W - 2.0 * PAD - (n - 1.0) * 6.0) / n;
        for (i, &v) in s.stages.iter().enumerate() {
            let r = rf(PAD + i as f32 * (cw + 6.0), 144.0, cw, 34.0);
            let on = i == s.active;
            let bg = if on { ACCENT } else if hov == Some(Act::Stage(i)) { CARD_HOVER } else { CARD };
            d.round(r.X, r.Y, r.Width, r.Height, 10.0, bg);
            d.text(&v.to_string(), 13.0, true, if on { BG } else { FG }, r, Align::Center);
            hits.push((r, Act::Stage(i)));
        }

        // частота опроса
        let polls = if self.wired { mouse::POLL_WIRED } else { mouse::POLL_DONGLE };
        let labels: Vec<(String, Act)> = polls.iter()
            .map(|&hz| (if hz >= 1000 { format!("{}K", hz / 1000) } else { hz.to_string() }, Act::Poll(hz))).collect();
        d.text("Частота опроса, Гц", 12.0, false, MUTED, rf(PAD, 194.0, 200.0, 18.0), Align::Left);
        segments(&d, 214.0, &labels, Act::Poll(s.poll), hov, &mut hits);

        // высота отрыва
        let labels: Vec<(String, Act)> = mouse::LODS.iter().map(|&(c, l)| (l.to_string(), Act::Lod(c))).collect();
        d.text("Высота отрыва", 12.0, false, MUTED, rf(PAD, 262.0, 200.0, 18.0), Align::Left);
        segments(&d, 282.0, &labels, Act::Lod(s.lod), hov, &mut hits);

        // сон
        let labels: Vec<(String, Act)> = mouse::SLEEP_S.iter().map(|&(v, l)| (l.to_string(), Act::Sleep(v))).collect();
        d.text("Засыпать через", 12.0, false, MUTED, rf(PAD, 330.0, 200.0, 18.0), Align::Left);
        segments(&d, 350.0, &labels, Act::Sleep(s.sleep_s), hov, &mut hits);

        // переключатели
        let rows = [("Motion Sync", "плавнее движение, чуть больше задержка", s.motion_sync, Act::Motion),
            ("Angle Snap", "выпрямляет линии — для рисования", s.angle_snap, Act::Angle)];
        for (k, (name, hint, on, act)) in rows.into_iter().enumerate() {
            let y = 402.0 + k as f32 * 46.0;
            let row = rf(PAD - 8.0, y - 4.0, W - 2.0 * PAD + 16.0, 42.0);
            if hov == Some(act) {
                d.round(row.X, row.Y, row.Width, row.Height, 10.0, CARD);
            }
            d.text(name, 14.0, false, FG, rf(PAD, y, 220.0, 20.0), Align::Left);
            d.text(hint, 11.0, false, MUTED, rf(PAD, y + 18.0, 250.0, 16.0), Align::Left);
            let (tx, ty) = (W - PAD - 40.0, y + 6.0);
            d.round(tx, ty, 40.0, 22.0, 11.0, if on { ACCENT } else { CARD_HOVER });
            d.ellipse(if on { tx + 21.0 } else { tx + 3.0 }, ty + 3.0, 16.0, 16.0, FG);
            hits.push((row, act));
        }

        // сброс к заводским: маленькая кнопка ↺ справа под зарядом; подпись под ней при наведении, сброс — со второго клика
        let armed = self.reset_armed;
        let hot = hov == Some(Act::Reset);
        let r = rf(W - PAD - 24.0, 40.0, 24.0, 24.0);
        d.round(r.X, r.Y, r.Width, r.Height, 12.0, if armed { RED } else if hot { CARD_HOVER } else { CARD });
        d.reset_icon(r.X + 12.0, r.Y + 12.0, if armed { FG } else if hot { RED } else { MUTED });
        if armed || hot {
            let label = if armed { "Нажми ещё раз — сброшу" } else { "Заводские настройки" };
            d.text(label, 11.0, armed, if armed { RED } else { MUTED }, rf(W - PAD - 200.0, 67.0, 200.0, 16.0), Align::Right);
        }
        hits.push((r, Act::Reset));
        self.hits = hits;
    }
}

// Ряд кнопок-сегментов во всю ширину; выбранная — бирюзовая
fn segments(d: &Draw, y: f32, items: &[(String, Act)], selected: Act, hover: Option<Act>, hits: &mut Vec<(RectF, Act)>) {
    let n = items.len() as f32;
    d.round(PAD, y, W - 2.0 * PAD, 34.0, 11.0, CARD);
    let cw = (W - 2.0 * PAD - 6.0) / n;
    for (i, (label, act)) in items.iter().enumerate() {
        let r = rf(PAD + 3.0 + i as f32 * cw, y + 3.0, cw, 28.0);
        let on = *act == selected;
        if on {
            d.round(r.X, r.Y, r.Width, r.Height, 9.0, ACCENT);
        } else if hover == Some(*act) {
            d.round(r.X, r.Y, r.Width, r.Height, 9.0, CARD_HOVER);
        }
        d.text(label, 12.0, on, if on { BG } else { FG }, r, Align::Center);
        hits.push((r, *act));
    }
}

// ───── GDI+ помощники ─────

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

struct Draw<'a> {
    g: *mut GpGraphics,
    f: &'a Fonts,
}

fn rf(x: f32, y: f32, w: f32, h: f32) -> RectF {
    RectF { X: x, Y: y, Width: w, Height: h }
}

impl Draw<'_> {
    fn path(x: f32, y: f32, w: f32, h: f32, r: f32) -> *mut GpPath {
        let d = (r * 2.0).min(w).min(h);
        let mut p = ptr::null_mut();
        unsafe {
            GdipCreatePath(FillModeAlternate, &mut p);
            GdipAddPathArc(p, x, y, d, d, 180.0, 90.0);
            GdipAddPathArc(p, x + w - d, y, d, d, 270.0, 90.0);
            GdipAddPathArc(p, x + w - d, y + h - d, d, d, 0.0, 90.0);
            GdipAddPathArc(p, x, y + h - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(p);
        }
        p
    }

    fn round(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: u32) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        unsafe {
            let p = Self::path(x, y, w, h, r);
            let mut b = ptr::null_mut();
            GdipCreateSolidFill(c, &mut b);
            GdipFillPath(self.g, b as *mut GpBrush, p);
            GdipDeleteBrush(b as *mut GpBrush);
            GdipDeletePath(p);
        }
    }

    fn round_outline(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: u32) {
        unsafe {
            let p = Self::path(x, y, w, h, r);
            let mut pen = ptr::null_mut();
            GdipCreatePen1(c, 1.0, UnitPixel, &mut pen);
            GdipDrawPath(self.g, pen, p);
            GdipDeletePen(pen);
            GdipDeletePath(p);
        }
    }

    fn ellipse(&self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        unsafe {
            let mut b = ptr::null_mut();
            GdipCreateSolidFill(c, &mut b);
            GdipFillEllipse(self.g, b as *mut GpBrush, x, y, w, h);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }

    // Значок «вернуть»: дуга со стрелкой, рисуем сами — в Segoe UI такого символа может не быть
    fn reset_icon(&self, cx: f32, cy: f32, c: u32) {
        let rad = 6.0f32;
        let (start, sweep) = (-60.0f32, -280.0f32); // против часовой, разрыв сверху справа
        unsafe {
            let mut pen = ptr::null_mut();
            GdipCreatePen1(c, 1.7, UnitWorld, &mut pen);
            GdipSetPenStartCap(pen, LineCapRound);
            GdipDrawArc(self.g, pen, cx - rad, cy - rad, 2.0 * rad, 2.0 * rad, start, sweep);
            GdipDeletePen(pen);
            // наконечник на конце дуги, по касательной в сторону движения
            let a = (start + sweep).to_radians();
            let (px, py) = (cx + rad * a.cos(), cy + rad * a.sin());
            let (dx, dy) = (a.sin(), -a.cos()); // касательная при обходе против часовой
            let (nx, ny) = (a.cos(), a.sin());
            let pts = [
                PointF { X: px + dx * 3.2, Y: py + dy * 3.2 },
                PointF { X: px - dx * 1.0 + nx * 3.0, Y: py - dy * 1.0 + ny * 3.0 },
                PointF { X: px - dx * 1.0 - nx * 3.0, Y: py - dy * 1.0 - ny * 3.0 },
            ];
            let mut b = ptr::null_mut();
            GdipCreateSolidFill(c, &mut b);
            GdipFillPolygon(self.g, b as *mut GpBrush, pts.as_ptr(), 3, FillModeAlternate);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }

    // Ширина строки — чтобы поставить «›» сразу за именем профиля
    fn measure(&self, s: &str, size: f32, bold: bool) -> f32 {
        unsafe {
            let mut font = ptr::null_mut();
            GdipCreateFont(if bold { self.f.semibold } else { self.f.regular }, size, 0, UnitPixel, &mut font);
            let ws: Vec<u16> = s.encode_utf16().collect();
            let mut out = rf(0.0, 0.0, 0.0, 0.0);
            GdipMeasureString(self.g, ws.as_ptr(), ws.len() as i32, font, &rf(0.0, 0.0, 1000.0, 100.0), ptr::null_mut(),
                &mut out, ptr::null_mut(), ptr::null_mut());
            GdipDeleteFont(font);
            out.Width
        }
    }

    fn text(&self, s: &str, size: f32, bold: bool, c: u32, r: RectF, align: Align) {
        unsafe {
            let mut font = ptr::null_mut();
            GdipCreateFont(if bold { self.f.semibold } else { self.f.regular }, size, 0, UnitPixel, &mut font);
            let mut fmt = ptr::null_mut();
            GdipCreateStringFormat(0, 0, &mut fmt);
            GdipSetStringFormatAlign(fmt, match align {
                Align::Left => StringAlignmentNear,
                Align::Center => StringAlignmentCenter,
                Align::Right => StringAlignmentFar,
            });
            GdipSetStringFormatLineAlign(fmt, if matches!(align, Align::Center) { StringAlignmentCenter } else { StringAlignmentNear });
            GdipSetStringFormatTrimming(fmt, StringTrimmingEllipsisCharacter);
            let mut b = ptr::null_mut();
            GdipCreateSolidFill(c, &mut b);
            let ws: Vec<u16> = s.encode_utf16().collect();
            GdipDrawString(self.g, ws.as_ptr(), ws.len() as i32, font, &r, fmt, b as *mut GpBrush);
            GdipDeleteBrush(b as *mut GpBrush);
            GdipDeleteStringFormat(fmt);
            GdipDeleteFont(font);
        }
    }
}

// Кадр «джинна» прямо в пиксели окна: каждая строка снимка сжимается по ширине к иконке.
// Чем ниже строка и чем раньше в анимации, тем сильнее стянута — бока выгибаются воронкой, панель вытекает снизу вверх.
// Свой цикл вместо сотен вызовов GDI+ на кадр: ~10 мс → пара мс, и строки по 1 пикселю, а не по 2.
// dst уже очищен; возвращает общую прозрачность кадра.
fn genie_frame(dst: &mut [u32], dw: usize, src: &[u32], sw: usize, (px, py): (f32, f32), (ax, ay): (f32, f32), t: f32) -> u8 {
    let sh = src.len() / sw.max(1);
    let dh = dst.len() / dw.max(1);
    let (pw, ph) = (sw as f32, sh as f32);
    let top = lerp(ay, py, smooth(clamp01(t / 0.8))); // верх догоняет позже
    let bot = lerp(ay, py + ph, smooth(clamp01(t / 0.45))); // низ быстро отрывается от иконки
    let squeeze = 1.0 - smooth(clamp01((t - 0.25) / 0.75)); // 1 — бока стянуты в иконку, 0 — прямые
    let span = (ay - py).max(1.0);
    if bot - top < 1.0 || sh == 0 {
        return 0;
    }
    let y0 = top.floor().max(0.0) as usize;
    let y1 = (bot.ceil().max(0.0) as usize).min(dh);
    for y in y0..y1 {
        let yc = y as f32 + 0.5;
        if yc < top || yc >= bot {
            continue;
        }
        let f = smooth(clamp01((yc - py) / span)) * squeeze;
        let l = lerp(px, ax - 4.0, f);
        let r = lerp(px + pw, ax + 4.0, f);
        if r - l < 0.5 {
            continue;
        }
        // строка снимка (с подмешиванием соседней — без лесенки при сжатии по высоте)
        let sy = ((yc - top) / (bot - top) * ph - 0.5).clamp(0.0, ph - 1.0);
        let (sy0, fy) = (sy as usize, sy.fract());
        let sy1 = (sy0 + 1).min(sh - 1);
        let (row0, row1) = (&src[sy0 * sw..][..sw], &src[sy1 * sw..][..sw]);
        let k = pw / (r - l);
        let x0 = l.floor().max(0.0) as usize;
        let x1 = (r.ceil().max(0.0) as usize).min(dw);
        let out = &mut dst[y * dw..][..dw];
        for x in x0..x1 {
            let xc = x as f32 + 0.5;
            let cover = clamp01((xc + 0.5).min(r) - (xc - 0.5).max(l)); // сглаживание краёв воронки
            if cover <= 0.0 {
                continue;
            }
            let sx = ((xc - l) * k - 0.5).clamp(0.0, pw - 1.0);
            let (sx0, fx) = (sx as usize, sx.fract());
            let sx1 = (sx0 + 1).min(sw - 1);
            out[x] = mix4(row0[sx0], row0[sx1], row1[sx0], row1[sx1], fx, fy, cover);
        }
    }
    (255.0 * clamp01(t / 0.12)) as u8
}

// Билинейное смешивание четырёх пикселей PARGB, умноженное на покрытие
fn mix4(a: u32, b: u32, c: u32, d: u32, fx: f32, fy: f32, cover: f32) -> u32 {
    let (wa, wb, wc, wd) = ((1.0 - fx) * (1.0 - fy) * cover, fx * (1.0 - fy) * cover, (1.0 - fx) * fy * cover, fx * fy * cover);
    let mut out = 0u32;
    for sh in [0, 8, 16, 24] {
        let ch = |p: u32| ((p >> sh) & 0xFF) as f32;
        let v = ch(a) * wa + ch(b) * wb + ch(c) * wc + ch(d) * wd;
        out |= ((v + 0.5) as u32).min(255) << sh;
    }
    out
}

fn lerp(a: f32, b: f32, k: f32) -> f32 { a + (b - a) * k }
fn clamp01(v: f32) -> f32 { v.clamp(0.0, 1.0) }
fn smooth(k: f32) -> f32 { k * k * (3.0 - 2.0 * k) }

// ───── система ─────

fn monitor_rects(pt: POINT) -> (RECT, RECT) {
    unsafe {
        let m = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = mem::zeroed();
        mi.cbSize = mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(m, &mut mi);
        (mi.rcWork, mi.rcMonitor)
    }
}

// Всплывашка скрытых значков трея (стрелка ^ у часов), если она сейчас открыта
fn overflow_rect() -> Option<RECT> {
    unsafe {
        let h = FindWindowW(w("NotifyIconOverflowWindow").as_ptr(), ptr::null());
        if h.is_null() || IsWindowVisible(h) == 0 {
            return None;
        }
        let mut r: RECT = mem::zeroed();
        (GetWindowRect(h, &mut r) != 0).then_some(r)
    }
}

// Windows не даёт просто так забрать фокус у чужого окна; на время подключаемся к очереди ввода активного окна
fn force_foreground(h: HWND) {
    unsafe {
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, ptr::null_mut());
        let me = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0;
        SetForegroundWindow(h);
        BringWindowToTop(h);
        SetFocus(h);
        if attached {
            AttachThreadInput(me, fg_thread, 0);
        }
    }
}

// Анимация кадр за кадром в такт обновлению экрана (DwmFlush ждёт следующий кадр композитора).
// Таймер Windows тикает с шагом 15,6 мс и сбивается, отсюда были рывки. Сообщения в это время продолжают обрабатываться.
unsafe fn anim_loop() {
    if LOOPING.replace(true) {
        return;
    }
    while with(|p| p.animate()).unwrap_or(false) {
        if DwmFlush() < 0 {
            thread::sleep(Duration::from_millis(8)); // композитор недоступен — просто не грузим процессор
        }
        let mut msg: MSG = mem::zeroed();
        while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam as i32); // вернём выход основному циклу
                LOOPING.set(false);
                return;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    LOOPING.set(false);
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let done = match msg {
        WM_TIMER => with(|p| match wp {
            T_DPI => p.flush_dpi_now(),
            T_LEAVE => {
                KillTimer(p.hwnd, T_LEAVE);
                p.close();
            }
            T_RESET => {
                KillTimer(p.hwnd, T_RESET);
                p.reset_armed = false;
                p.render();
            }
            T_WATCH => {
                let fg = GetForegroundWindow();
                if p.visible && !p.closing && fg != p.hwnd && fg != p.fg_at_open {
                    p.close(); // активным стало другое окно — значит, кликнули мимо
                }
            }
            _ => {}
        }),
        WM_MOUSEMOVE => with(|p| p.mouse_move(lp)),
        WM_MOUSELEAVE => with(|p| {
            if p.entered && p.visible && !p.closing && p.editing.is_none() {
                SetTimer(p.hwnd, T_LEAVE, 350, None); // мимолётный выход за край не считаем
            }
            if p.hover.is_some() {
                p.hover = None;
                p.render();
            }
        }),
        WM_LBUTTONUP => with(|p| p.click(lp)),
        WM_LBUTTONDBLCLK => with(|p| p.dblclick(lp)),
        WM_CHAR => with(|p| p.char_in(wp as u32)).filter(|&used| used).map(|_| ()),
        WM_MOUSEWHEEL => with(|p| p.wheel(wp)),
        WM_KEYDOWN if wp == VK_ESCAPE as usize => with(|p| {
            if p.editing.is_none() {
                p.close(); // при вводе имени Esc отменяет ввод (через WM_CHAR), а не закрывает панель
            }
        }),
        WM_ANIM => {
            anim_loop();
            Some(())
        }
        WM_LOADED => with(|p| p.loaded()),
        WM_CLOSE_PANEL => with(|p| p.close()),
        WM_ACTIVATE => {
            if (wp & 0xFFFF) as u32 == WA_INACTIVE {
                PostMessageW(hwnd, WM_CLOSE_PANEL, 0, 0); // закрываем вне текущего вызова
            }
            None
        }
        WM_SETCURSOR => Some(()), // курсор ставим сами в WM_MOUSEMOVE
        _ => None,
    };
    if done.is_some() && msg != WM_ACTIVATE {
        return 0;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

impl Panel {
    // таймер дописи DPI сработал — запись уже ничего не ждёт
    fn flush_dpi_now(&mut self) {
        self.flush_dpi();
    }
}


// Имена профилей: по строке на профиль в %APPDATA%\R5BatteryTray\profiles.txt
fn names_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("R5BatteryTray").join("profiles.txt")
}

fn load_names() -> [String; 3] {
    let text = std::fs::read_to_string(names_path()).unwrap_or_default();
    let mut lines = text.lines();
    std::array::from_fn(|_| lines.next().unwrap_or("").trim().to_string())
}

fn save_names(names: &[String; 3]) {
    let p = names_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, names.join("
"));
}

#[cfg(test)]
mod tests {
    use super::*;

    // В конце анимации кадр — ровно снимок на своём месте; в начале — ничего
    #[test]
    fn genie_ends_as_snapshot() {
        let (sw, sh, dw, dh) = (6usize, 5usize, 10usize, 12usize);
        let src: Vec<u32> = (0..sw * sh).map(|i| 0xFF00_0000 | i as u32).collect();
        let mut dst = vec![0u32; dw * dh];
        assert_eq!(genie_frame(&mut dst, dw, &src, sw, (2.0, 3.0), (5.0, 11.0), 1.0), 255);
        for y in 0..sh {
            assert_eq!(&dst[(y + 3) * dw + 2..][..sw], &src[y * sw..][..sw]);
        }
        let mut dst = vec![0u32; dw * dh];
        assert_eq!(genie_frame(&mut dst, dw, &src, sw, (2.0, 3.0), (5.0, 11.0), 0.0), 0);
        assert!(dst.iter().all(|&p| p == 0));
    }
}

