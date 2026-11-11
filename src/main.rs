// Иконка в трее с зарядом Attack Shark R5 Ultra (VID 373E, PID 0046 провод / 0047 донгл).
// Протокол: feature-report 65 байт, запрос 00 00 00 02 02 00 83, ответ: [1]=A1, [7]=зарядка, [8]=процент.
// Чистый Win32 без рантаймов: скрытое окно + Shell_NotifyIcon, опрос раз в 30 с.
#![windows_subsystem = "windows"]

mod mouse;
mod panel;

use std::{cell::RefCell, mem, ptr, thread, time::Duration};
use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::*,
    Devices::HumanInterfaceDevice::*,
    Foundation::*,
    Graphics::Gdi::*,
    Storage::FileSystem::*,
    System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW},
    System::Registry::*,
    System::Threading::CreateMutexW,
    Networking::WinInet::DeleteUrlCacheEntryW,
    Security::Cryptography::*,
    System::Com::Urlmon::URLDownloadToFileW,
    UI::Shell::*,
    UI::WindowsAndMessaging::*,
};

const WM_TRAY: u32 = WM_APP + 1;
const ID_REFRESH: usize = 1;
const ID_AUTORUN: usize = 2;
const ID_EXIT: usize = 3;
const ID_UPDATE: usize = 4;
const REPO: &str = "STANFORDVIBE/r5-battery-tray";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP: &str = "R5BatteryTray";

struct State {
    hwnd: HWND,
    icon: HICON,
    tip: String,
    last_tip: Option<String>, // последний удачный ответ: спящая мышь заряд не отдаёт, держим старый
    good_path: Option<Vec<u16>>, // интерфейс, который уже ответил, чтобы не перебирать каждый раз
    taskbar_msg: u32,
    mutex: HANDLE, // отпускаем перед перезапуском после обновления, иначе новая копия решит, что уже запущена
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State {
        hwnd: ptr::null_mut(), icon: ptr::null_mut(), tip: String::new(),
        last_tip: None, good_path: None, taskbar_msg: 0, mutex: ptr::null_mut(),
    });
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn main() {
    unsafe {
        let mutex = CreateMutexW(ptr::null(), 1, w("R5BatteryTray_single").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let hinst = GetModuleHandleW(ptr::null());
        let cls = w(APP);
        let mut wc: WNDCLASSW = mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);
        // обычное (не message-only) скрытое окно: иначе не придёт TaskbarCreated после перезапуска проводника
        let hwnd = CreateWindowExW(0, cls.as_ptr(), cls.as_ptr(), 0, 0, 0, 0, 0,
            ptr::null_mut(), ptr::null_mut(), hinst, ptr::null());
        let tb = RegisterWindowMessageW(w("TaskbarCreated").as_ptr());
        ST.with(|s| { let mut s = s.borrow_mut(); s.hwnd = hwnd; s.taskbar_msg = tb; s.mutex = mutex; });
        remove_old_exe();
        SetProcessDPIAware();
        panel::init();

        update();
        notify(NIM_ADD);
        SetTimer(hwnd, 1, 30_000, None);

        let mut msg: MSG = mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let tb = ST.with(|s| s.borrow().taskbar_msg);
    match msg {
        WM_TIMER => { remove_old_exe(); update(); notify(NIM_MODIFY); }
        WM_TRAY => match lp as u32 {
            WM_LBUTTONUP => panel::toggle(),
            WM_RBUTTONUP => menu(hwnd),
            _ => {}
        },
        m if m == tb && tb != 0 => notify(NIM_ADD),
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

unsafe fn menu(hwnd: HWND) {
    let m = CreatePopupMenu();
    let auto = if autorun_get() { MF_CHECKED } else { MF_UNCHECKED };
    AppendMenuW(m, MF_STRING, ID_REFRESH, w("Обновить").as_ptr());
    AppendMenuW(m, MF_STRING | auto, ID_AUTORUN, w("Запускать с Windows").as_ptr());
    AppendMenuW(m, MF_STRING, ID_UPDATE, w(&format!("Проверить обновления (v{VERSION})")).as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, ptr::null());
    AppendMenuW(m, MF_STRING, ID_EXIT, w("Выход").as_ptr());
    let mut pt: POINT = mem::zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // без этого меню не закрывается кликом мимо
    let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, ptr::null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(m);
    match cmd as usize {
        ID_REFRESH => { update(); notify(NIM_MODIFY); }
        ID_AUTORUN => autorun_set(!autorun_get()),
        ID_UPDATE => check_update(hwnd),
        ID_EXIT => { notify(NIM_DELETE); PostQuitMessage(0); }
        _ => {}
    }
}

// Читает заряд и готовит иконку/подсказку в State; показывает их notify().
fn update() {
    let r = read_battery();
    ST.with(|s| {
        let mut s = s.borrow_mut();
        match r {
            Some((pct, charging)) => {
                let tip = format!("R5 Ultra: {pct}%{}", if charging { ", заряжается" } else { "" });
                set_icon(&mut s, make_icon(pct, charging));
                s.tip = tip.clone();
                s.last_tip = Some(tip);
            }
            None => match s.last_tip.clone() {
                Some(t) => s.tip = format!("{t} (мышь спит)"),
                None => {
                    set_icon(&mut s, make_icon(0, false));
                    s.tip = "R5 Ultra: мышь спит или выключена — пошевели её".into();
                }
            },
        }
    });
}

fn set_icon(s: &mut State, icon: HICON) {
    let old = mem::replace(&mut s.icon, icon);
    if !old.is_null() {
        unsafe { DestroyIcon(old) }; // трей держит свою копию иконки, нашу старую можно удалять
    }
}

fn notify(action: NOTIFY_ICON_MESSAGE) {
    ST.with(|s| unsafe {
        let s = s.borrow();
        let mut nid: NOTIFYICONDATAW = mem::zeroed();
        nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = s.hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = s.icon;
        for (d, c) in nid.szTip.iter_mut().zip(s.tip.encode_utf16().take(127)) {
            *d = c;
        }
        Shell_NotifyIconW(action, &nid);
    });
}

// ---------- мышь ----------

fn read_battery() -> Option<(u8, bool)> {
    // панель сейчас говорит с мышью — не ждём её, иконка обновится в следующий раз
    let Ok(_l) = mouse::HID_LOCK.try_lock() else { return None };
    let cached = ST.with(|s| s.borrow().good_path.clone());
    if let Some(p) = cached {
        if let Some(r) = query(&p) {
            return Some(r);
        }
    }
    for p in hid_paths() {
        if let Some(r) = query(&p) {
            ST.with(|s| s.borrow_mut().good_path = Some(p));
            return Some(r);
        }
    }
    ST.with(|s| s.borrow_mut().good_path = None);
    None
}

// Пути всех HID-интерфейсов мыши (донгл и провод).
fn hid_paths() -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    unsafe {
        let mut guid = mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, ptr::null(), ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set as isize == -1 {
            return out;
        }
        let mut i = 0;
        loop {
            let mut ifd: SP_DEVICE_INTERFACE_DATA = mem::zeroed();
            ifd.cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, ptr::null(), &guid, i, &mut ifd) == 0 {
                break;
            }
            i += 1;
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &ifd, ptr::null_mut(), 0, &mut need, ptr::null_mut());
            if need == 0 {
                continue;
            }
            let mut buf = vec![0u64; need as usize / 8 + 1]; // u64 — ради выравнивания структуры
            let det = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*det).cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &ifd, det, need, ptr::null_mut(), ptr::null_mut()) == 0 {
                continue;
            }
            let p = ptr::addr_of!((*det).DevicePath) as *const u16;
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let path = std::slice::from_raw_parts(p, len);
            if String::from_utf16_lossy(path).to_ascii_lowercase().contains("vid_373e&pid_004") {
                let mut v = path.to_vec();
                v.push(0);
                out.push(v);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

fn query(path: &[u16]) -> Option<(u8, bool)> {
    unsafe {
        let h = CreateFileW(path.as_ptr(), 0xC000_0000, FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(), OPEN_EXISTING, 0, ptr::null_mut());
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut req = [0u8; 65];
        req[3] = 0x02;
        req[4] = 0x02;
        req[6] = 0x83;
        let mut result = None;
        if HidD_SetFeature(h, req.as_ptr() as _, 65) != 0 {
            // A0 = донгл ещё ждёт мышь (или она спит) — немного подождём
            for _ in 0..6 {
                thread::sleep(Duration::from_millis(60));
                let mut res = [0u8; 65];
                if HidD_GetFeature(h, res.as_mut_ptr() as _, 65) == 0 {
                    break;
                }
                if let Some(r) = parse(&res) {
                    result = Some(r);
                    break;
                }
            }
        }
        CloseHandle(h);
        result
    }
}

fn parse(res: &[u8; 65]) -> Option<(u8, bool)> {
    (res[1] == 0xA1 && res[4] == 0x02 && res[6] == 0x83 && res[8] <= 100).then(|| (res[8], res[7] != 0))
}

// ---------- иконка ----------

// Мышь пиксель-артом 16×16 в стиле KbBatteryTray: белый контур, две кнопки, под ними корпус.
// Заряд — корпус заливается снизу вверх, как уровень жидкости: зелёным (≤20% — красным), остальное серое.
// Зарядка — маленькая жёлтая молния в правом верхнем углу.
// shortcut: масштаб только целый (16→32), на 20/24 px рисунок 16 px по центру — мельче, зато чёткий.
// Строки корпуса под кнопками сверху вниз: (x, ширина) заливки.
const FILL: [(i32, i32); 7] = [(3, 9), (3, 9), (3, 9), (3, 9), (3, 9), (3, 9), (4, 7)];
const FILL_TOP: i32 = 7;

// Молния пиксель-артом 5×7.
const BOLT: [&str; 7] = ["...##", "..##.", ".##..", "#####", "..##.", ".##..", "##..."];

fn icon_pixels(s: i32, pct: u8, charging: bool) -> Vec<u32> {
    const WHITE: u32 = 0xFFFF_FFFF;
    const GREY: u32 = 0xFF6E_6E6E;
    const YELLOW: u32 = 0xFFFF_D60A;
    const DARK: u32 = 0xFF1E_1E1E;
    let lit = if pct <= 20 { 0xFFFF_3B30 } else { 0xFF34_C759 };
    let f = (s / 16).max(1);
    let o = (s - 16 * f) / 2;
    let mut px = vec![0u32; (s * s) as usize];
    let mut rect = |x: i32, y: i32, w: i32, h: i32, c: u32| {
        for yy in (o + y * f).max(0)..(o + (y + h) * f).min(s) {
            for xx in (o + x * f).max(0)..(o + (x + w) * f).min(s) {
                px[(yy * s + xx) as usize] = c;
            }
        }
    };
    // контур 11×14 (x 2..12, y 1..14), углы срезаны ступенькой
    rect(5, 1, 5, 1, WHITE);
    rect(3, 2, 2, 1, WHITE);
    rect(10, 2, 2, 1, WHITE);
    rect(7, 2, 1, 4, WHITE); // щель между кнопками
    rect(2, 3, 1, 10, WHITE);
    rect(12, 3, 1, 10, WHITE);
    rect(2, 6, 11, 1, WHITE); // низ кнопок
    rect(3, 13, 1, 1, WHITE);
    rect(11, 13, 1, 1, WHITE);
    rect(4, 14, 7, 1, WHITE);
    // сколько строк залито, считая снизу; любой ненулевой заряд — хотя бы одна
    let n = (FILL.len() * pct as usize).div_ceil(100);
    for (i, (x, w)) in FILL.into_iter().enumerate() {
        let on = FILL.len() - i <= n;
        rect(x, FILL_TOP + i as i32, w, 1, if on { lit } else { GREY });
    }
    if charging {
        let (bx, by) = (11, 0); // правый верхний угол, как бейдж
        let inb = |x: i32, y: i32| {
            (0..7).contains(&(y - by)) && (0..5).contains(&(x - bx))
                && BOLT[(y - by) as usize].as_bytes()[(x - bx) as usize] == b'#'
        };
        for y in -1..17 {
            for x in -1..17 {
                if inb(x, y) {
                    rect(x, y, 1, 1, YELLOW);
                } else if [(0, -1), (0, 1), (-1, 0), (1, 0)].iter().any(|&(dx, dy)| inb(x + dx, y + dy)) {
                    rect(x, y, 1, 1, DARK); // обводка, чтобы молния не сливалась с корпусом
                }
            }
        }
    }
    px
}

fn make_icon(pct: u8, charging: bool) -> HICON {
    unsafe {
        let s = GetSystemMetrics(SM_CXSMICON).max(16);
        let px = icon_pixels(s, pct, charging);
        let mut bi: BITMAPINFO = mem::zeroed();
        bi.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = s;
        bi.bmiHeader.biHeight = -s; // сверху вниз
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut bits: *mut core::ffi::c_void = ptr::null_mut();
        let color = CreateDIBSection(ptr::null_mut(), &bi, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
        ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(s, s, 1, 1, ptr::null()); // прозрачность берётся из альфа-канала
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

// ---------- обновление ----------
// Только по кнопке в меню: смотрим последний релиз на GitHub, качаем exe и его подпись, подменяем себя и перезапускаемся.
// Подпись (ECDSA P-256 + SHA-256) сверяем с вшитым открытым ключом: даже со взломанным GitHub чужой exe не встанет.
// Закрытый ключ — только у автора (scripts/sign-release.ps1).
const RELEASE_PUB: &[u8] = include_bytes!("../release.pub");
// shortcut: качаем прямо в потоке окна — пару секунд иконка не реагирует; хватает, пока exe маленький.

fn download(url: &str, to: &std::path::Path) -> bool {
    let (u, f) = (w(url), w(&to.to_string_lossy()));
    unsafe {
        DeleteUrlCacheEntryW(u.as_ptr()); // иначе Windows может отдать старый ответ из кэша
        URLDownloadToFileW(ptr::null_mut(), u.as_ptr(), f.as_ptr(), 0, ptr::null_mut()) == 0
    }
}

fn message(hwnd: HWND, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(hwnd, w(text).as_ptr(), w("Заряд мыши").as_ptr(), flags) }
}

fn check_update(hwnd: HWND) {
    let json = std::env::temp_dir().join("R5BatteryTray-latest.json");
    let tag = download(&format!("https://api.github.com/repos/{REPO}/releases/latest"), &json)
        .then(|| std::fs::read_to_string(&json).ok())
        .flatten()
        .and_then(|s| latest_tag(&s));
    let _ = std::fs::remove_file(&json);
    let Some(tag) = tag else {
        message(hwnd, "Не получилось проверить обновления. Есть интернет?", MB_ICONWARNING);
        return;
    };
    if !newer(&tag, VERSION) {
        message(hwnd, &format!("Стоит последняя версия v{VERSION}."), MB_ICONINFORMATION);
        return;
    }
    let ask = format!("Вышла новая версия {tag} (у тебя v{VERSION}). Обновить?");
    if message(hwnd, &ask, MB_YESNO | MB_ICONQUESTION) == IDYES {
        if let Err(e) = install() {
            message(hwnd, &format!("Не получилось обновиться: {e}"), MB_ICONWARNING);
        }
    }
}

// Качает новый exe рядом, подменяет текущий (запущенный exe Windows переименовать даёт) и перезапускает.
fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = exe.with_file_name("R5BatteryTray.new.exe");
    let old = exe.with_file_name("R5BatteryTray.old.exe");
    let url = format!("https://github.com/{REPO}/releases/latest/download/R5BatteryTray.exe");
    if !download(&url, &new) {
        return Err("файл не скачался".into());
    }
    // вместо exe может прийти страница с ошибкой — проверяем, что это программа
    let ok = std::fs::read(&new).map(|b| b.len() > 50_000 && b.starts_with(b"MZ")).unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(&new);
        return Err("скачался не тот файл".into());
    }
    let sig_file = exe.with_file_name("R5BatteryTray.new.exe.sig");
    let signed = download(&format!("{url}.sig"), &sig_file)
        && match (std::fs::read(&new), std::fs::read(&sig_file)) {
            (Ok(data), Ok(sig)) => signature_ok(RELEASE_PUB, &data, &sig),
            _ => false,
        };
    let _ = std::fs::remove_file(&sig_file);
    if !signed {
        let _ = std::fs::remove_file(&new);
        return Err("подпись не сходится — это не официальная версия, ставить не буду".into());
    }
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // вернуть как было
        return Err(e.to_string());
    }
    ST.with(|s| unsafe { CloseHandle(s.borrow().mutex) });
    if let Err(e) = std::process::Command::new(&exe).spawn() {
        return Err(format!("новая версия не запустилась: {e}")); // exe уже новый — запустится при следующем старте
    }
    notify(NIM_DELETE);
    unsafe { PostQuitMessage(0) };
    Ok(())
}

fn signature_ok(pubkey: &[u8], data: &[u8], sig: &[u8]) -> bool {
    unsafe {
        let mut hash = [0u8; 32];
        if BCryptHash(BCRYPT_SHA256_ALG_HANDLE, ptr::null(), 0, data.as_ptr(), data.len() as u32, hash.as_mut_ptr(), 32) != 0 {
            return false;
        }
        let mut alg = ptr::null_mut();
        if BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_ECDSA_P256_ALGORITHM, ptr::null(), 0) != 0 {
            return false;
        }
        let mut key = ptr::null_mut();
        let mut ok = false;
        if BCryptImportKeyPair(alg, ptr::null_mut(), BCRYPT_ECCPUBLIC_BLOB, &mut key, pubkey.as_ptr(), pubkey.len() as u32, 0) == 0 {
            ok = BCryptVerifySignature(key, ptr::null(), hash.as_ptr(), 32, sig.as_ptr(), sig.len() as u32, 0) == 0;
            BCryptDestroyKey(key);
        }
        BCryptCloseAlgorithmProvider(alg, 0);
        ok
    }
}

// Старый exe остаётся после обновления; удалить получится, когда прежняя копия закроется.
fn remove_old_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_file_name("R5BatteryTray.old.exe"));
    }
}

fn latest_tag(json: &str) -> Option<String> {
    let rest = &json[json.find("\"tag_name\"")? + 10..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
}

// «v0.3.0» новее «0.2.1»?
fn newer(tag: &str, current: &str) -> bool {
    let parse = |v: &str| v.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect::<Vec<u32>>();
    parse(tag) > parse(current)
}

// ---------- автозапуск ----------

fn autorun_get() -> bool {
    unsafe {
        let mut k: HKEY = ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), 0, KEY_QUERY_VALUE, &mut k) != ERROR_SUCCESS {
            return false;
        }
        let ok = RegQueryValueExW(k, w(APP).as_ptr(), ptr::null(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
            == ERROR_SUCCESS;
        RegCloseKey(k);
        ok
    }
}

fn autorun_set(on: bool) {
    unsafe {
        let mut k: HKEY = ptr::null_mut();
        if RegCreateKeyExW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), 0, ptr::null(), 0, KEY_SET_VALUE,
            ptr::null(), &mut k, ptr::null_mut()) != ERROR_SUCCESS
        {
            return;
        }
        if on {
            let mut buf = [0u16; 1024];
            let n = GetModuleFileNameW(ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) as usize;
            let val = w(&format!("\"{}\"", String::from_utf16_lossy(&buf[..n])));
            RegSetValueExW(k, w(APP).as_ptr(), 0, REG_SZ, val.as_ptr() as *const u8, (val.len() * 2) as u32);
        } else {
            RegDeleteValueW(k, w(APP).as_ptr());
        }
        RegCloseKey(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn signature_check() {
        // одноразовый тестовый ключ, подпись сделана .NET ECDsaCng — как в sign-release.ps1
        let pubk = unhex("4543533120000000045c965b062a2ee5d043b343040748bbe0fa178354c354c19ed5e67e691fd7a2eceeb80dad190444672295719a10b33f928ae4a780e16430c5d4d252ece433cf");
        let sig = unhex("3427ebe0cc59a334f43be37761254ed02b3abfa231795d0447f937a15df42dc584319dad480609779b913305791525e300ec7f514835d3b62372a6a2bde1eea8");
        assert!(signature_ok(&pubk, b"KbBatteryTray test", &sig));
        assert!(!signature_ok(&pubk, b"KbBatteryTray tesT", &sig)); // подменённый файл
        assert!(!signature_ok(RELEASE_PUB, b"KbBatteryTray test", &sig)); // чужой ключ
        assert!(!signature_ok(&pubk, b"KbBatteryTray test", &sig[..10])); // обрезанная подпись
        assert_eq!(RELEASE_PUB.len(), 72);
    }

    #[test]
    fn update_version_logic() {
        let json = r#"{"url":"x","tag_name":"v0.3.0","name":"0.3.0"}"#;
        assert_eq!(latest_tag(json).as_deref(), Some("v0.3.0"));
        assert_eq!(latest_tag("{}"), None);
        assert!(newer("v0.3.0", "0.2.0"));
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(!newer("v0.2.0", "0.2.0"));
        assert!(!newer("v0.1.5", "0.2.0"));
    }

    #[test]
    fn parses_battery_reply() {
        let mut r = [0u8; 65];
        r[1] = 0xA1; r[3] = 2; r[4] = 2; r[6] = 0x83; r[8] = 69;
        assert_eq!(parse(&r), Some((69, false)));
        r[7] = 1;
        assert_eq!(parse(&r), Some((69, true)));
        r[1] = 0xA0; // мышь спит
        assert_eq!(parse(&r), None);
    }

    // Превью всех состояний на тёмном фоне трея: cargo test preview -- --ignored, смотреть preview.bmp
    #[test]
    #[ignore]
    fn preview() {
        let states = [(0, false), (15, false), (60, false), (100, false), (15, true), (60, true)];
        let sizes = [16, 20, 24, 32];
        let (cell, pad) = (128usize, 8usize);
        let (w, h) = (states.len() * (cell + pad) + pad, sizes.len() * (cell + pad) + pad);
        let mut img = vec![0x2020_20u32; w * h];
        for (r, &s) in sizes.iter().enumerate() {
            for (c, &(pct, ch)) in states.iter().enumerate() {
                let px = icon_pixels(s, pct, ch);
                for y in 0..cell {
                    for x in 0..cell {
                        let p = px[(y * s as usize / cell) * s as usize + x * s as usize / cell];
                        if p >> 24 != 0 {
                            img[(pad + r * (cell + pad) + y) * w + pad + c * (cell + pad) + x] = p & 0xFF_FFFF;
                        }
                    }
                }
            }
        }
        let row = (w * 3 + 3) & !3;
        let mut f = Vec::new();
        f.extend(b"BM");
        f.extend(((54 + row * h) as u32).to_le_bytes());
        f.extend([0u8; 4]);
        f.extend(54u32.to_le_bytes());
        f.extend(40u32.to_le_bytes());
        f.extend((w as i32).to_le_bytes());
        f.extend((-(h as i32)).to_le_bytes());
        f.extend(1u16.to_le_bytes());
        f.extend(24u16.to_le_bytes());
        f.extend([0u8; 24]);
        for y in 0..h {
            for x in 0..w {
                f.extend(&img[y * w + x].to_le_bytes()[..3]);
            }
            f.resize(f.len() + row - w * 3, 0);
        }
        std::fs::write("preview.bmp", f).unwrap();
    }

    #[test]
    fn fill_grows_with_charge() {
        let green = |pct| icon_pixels(16, pct, false).iter().filter(|&&c| c == 0xFF34_C759).count();
        assert_eq!(green(0), 0);
        assert!(green(30) < green(60) && green(60) < green(100));
    }
}
