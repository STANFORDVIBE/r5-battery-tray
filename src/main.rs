// Иконка в трее с зарядом Attack Shark R5 Ultra (VID 373E, PID 0046 провод / 0047 донгл).
// Протокол: feature-report 65 байт, запрос 00 00 00 02 02 00 83, ответ: [1]=A1, [7]=зарядка, [8]=процент.
// Чистый Win32 без рантаймов: скрытое окно + Shell_NotifyIcon, опрос раз в 30 с.
#![windows_subsystem = "windows"]

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
    UI::Shell::*,
    UI::WindowsAndMessaging::*,
};

const WM_TRAY: u32 = WM_APP + 1;
const ID_REFRESH: usize = 1;
const ID_AUTORUN: usize = 2;
const ID_EXIT: usize = 3;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP: &str = "R5BatteryTray";

struct State {
    hwnd: HWND,
    icon: HICON,
    tip: String,
    last_tip: Option<String>, // последний удачный ответ: спящая мышь заряд не отдаёт, держим старый
    good_path: Option<Vec<u16>>, // интерфейс, который уже ответил, чтобы не перебирать каждый раз
    taskbar_msg: u32,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State {
        hwnd: ptr::null_mut(), icon: ptr::null_mut(), tip: String::new(),
        last_tip: None, good_path: None, taskbar_msg: 0,
    });
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn main() {
    unsafe {
        CreateMutexW(ptr::null(), 1, w("R5BatteryTray_single").as_ptr());
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
        ST.with(|s| { let mut s = s.borrow_mut(); s.hwnd = hwnd; s.taskbar_msg = tb; });

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
        WM_TIMER => { update(); notify(NIM_MODIFY); }
        WM_TRAY => match lp as u32 {
            WM_LBUTTONUP => { update(); notify(NIM_MODIFY); }
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

// Горизонтальная батарейка: белый контур, заливка по проценту.
// Зелёная — норм, жёлтая — заряжается, красная — ≤20%. pct=0 — пустая.
fn battery_pixels(s: i32, pct: u8, charging: bool) -> Vec<u32> {
    const WHITE: u32 = 0xFFFF_FFFF;
    let fill = if charging { 0xFFFF_D700 } else if pct <= 20 { 0xFFFF_0000 } else { 0xFF32_CD32 };
    let pen = (s / 16).max(1);
    let h = s * 9 / 16;
    let top = (s - h) / 2;
    let body_w = s - s / 8;
    let mut px = vec![0u32; (s * s) as usize];
    let mut rect = |x0: i32, y0: i32, w: i32, hh: i32, c: u32| {
        for y in y0.max(0)..(y0 + hh).min(s) {
            for x in x0.max(0)..(x0 + w).min(s) {
                px[(y * s + x) as usize] = c;
            }
        }
    };
    rect(0, top, body_w, pen, WHITE); // корпус
    rect(0, top + h - pen, body_w, pen, WHITE);
    rect(0, top, pen, h, WHITE);
    rect(body_w - pen, top, pen, h, WHITE);
    rect(body_w, top + h / 4, s - body_w, h - h / 2, WHITE); // носик
    let inner = body_w - 4 * pen;
    let fw = if pct == 0 { 0 } else { ((inner * pct as i32 + 50) / 100).max(pen) };
    rect(2 * pen, top + 2 * pen, fw, h - 4 * pen, fill);
    px
}

fn make_icon(pct: u8, charging: bool) -> HICON {
    unsafe {
        let s = GetSystemMetrics(SM_CXSMICON).max(16);
        let px = battery_pixels(s, pct, charging);
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

    #[test]
    fn fill_grows_with_charge() {
        let green = |pct| battery_pixels(16, pct, false).iter().filter(|&&c| c == 0xFF32_CD32).count();
        assert_eq!(green(0), 0);
        assert!(green(30) < green(69) && green(69) < green(100));
    }
}
