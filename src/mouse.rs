// Настройки мыши по протоколу официального веб-драйвера Attack Shark (xvalleyinno.top/AttackShark).
// Запрос — feature-report 65 байт: [0]=0 (номер отчёта), дальше 64 байта драйвера со сдвигом на 1:
// [3]=2 (кому: мышь), [4]=длина данных, [5]=1, [6]=команда (чтение = 0x80|код), [7]=профиль, [8..]=значения.
// Ответ: [1]=A1 — готово, меньше — мышь ещё думает (спросить ещё раз), больше — повторить запрос.
// Паспорт R5 Ultra из env-models.json драйвера: новый протокол (профиль в каждом запросе), DPI до 42000,
// 6 ступеней, LOD 0.7/1/2 мм, опрос 125–1000 по проводу и до 8000 через донгл.
use std::{thread, time::Duration};
use windows_sys::Win32::{Devices::HumanInterfaceDevice::*, Foundation::*, Storage::FileSystem::*};

pub const DPI_MAX: u16 = 42000;
pub const DPI_STEP: u16 = 50;
pub const MAX_STAGES: usize = 6;
pub const POLL_WIRED: &[u16] = &[125, 250, 500, 1000];
pub const POLL_DONGLE: &[u16] = &[125, 250, 500, 1000, 2000, 4000, 8000];
pub const LODS: &[(u8, &str)] = &[(0x87, "0.7 мм"), (1, "1 мм"), (2, "2 мм")]; // 0.7 мм драйвер шлёт как 7|0x80
pub const SLEEP_S: &[(u16, &str)] = &[(15, "15 с"), (30, "30 с"), (60, "1 мин"), (300, "5 мин"), (600, "10 мин")]; // мышь хранит секунды

// Один запрос к мыши за раз: заряд (окно) и настройки (фоновый поток) иначе перепутают ответы
pub static HID_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    HID_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub profile: u8,
    pub stages: Vec<u16>, // DPI ступеней (X; Y держим таким же)
    pub active: usize,    // активная ступень, с нуля
    pub poll: u16,        // Гц
    pub lod: u8,          // код как в LODS
    pub sleep_s: u16,
    pub motion_sync: bool,
    pub angle_snap: bool,
}

// Гц ↔ код частоты опроса из драйвера
pub fn poll_code(hz: u16) -> u8 {
    match hz { 125 => 8, 250 => 4, 500 => 2, 2000 => 32, 4000 => 64, 8000 => 128, _ => 1 }
}
pub fn poll_hz(code: u8) -> u16 {
    match code { 8 => 125, 4 => 250, 2 => 500, 32 => 2000, 64 => 4000, 128 => 8000, _ => 1000 } // 16 драйвер тоже считает 1000
}

pub struct Dev(HANDLE);

impl Drop for Dev {
    fn drop(&mut self) { unsafe { CloseHandle(self.0) }; }
}

impl Dev {
    pub fn open(path: &[u16]) -> Option<Dev> {
        let h = unsafe {
            CreateFileW(path.as_ptr(), 0xC000_0000, FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut())
        };
        (h != INVALID_HANDLE_VALUE).then_some(Dev(h))
    }

    fn set(&self, req: &[u8; 65]) -> bool {
        unsafe { HidD_SetFeature(self.0, req.as_ptr() as _, 65) != 0 }
    }

    fn get(&self) -> Option<[u8; 65]> {
        let mut res = [0u8; 65];
        unsafe { (HidD_GetFeature(self.0, res.as_mut_ptr() as _, 65) != 0).then_some(res) }
    }

    // Отправить и дождаться ответа A1 с тем же кодом команды — как retrySetGet в драйвере
    pub fn cmd(&self, len: u8, sub: u8, code: u8, data: &[u8]) -> Option<[u8; 65]> {
        let mut req = [0u8; 65];
        req[3] = 2;
        req[4] = len;
        req[5] = sub;
        req[6] = code;
        req[7..7 + data.len()].copy_from_slice(data);
        for _ in 0..4 {
            if !self.set(&req) {
                return None;
            }
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(25));
                let r = self.get()?;
                if r[1] == 0xA1 && r[6] == code {
                    return Some(r);
                }
                if r[1] > 0xA1 {
                    break; // мышь просит повторить
                }
            }
        }
        None
    }

    pub fn read(&self) -> Option<Settings> {
        let profile = self.cmd(1, 0, 0x85, &[])?[7].max(1);
        let p = profile;
        let st = self.cmd(10, 1, 0x81, &[p, MAX_STAGES as u8])?;
        let n = (st[8] as usize).clamp(1, MAX_STAGES);
        let stages = (0..n).map(|i| u16::from_be_bytes([st[9 + i * 4], st[10 + i * 4]])).collect();
        let active = (self.cmd(2, 1, 0x82, &[p])?[8] as usize).clamp(1, n) - 1;
        let poll = poll_hz(self.cmd(2, 1, 0x80, &[p])?[8]);
        let lod = self.cmd(2, 1, 0x88, &[p])?[8];
        let s = self.cmd(3, 0, 0x87, &[p])?;
        let sleep_s = u16::from_be_bytes([s[8], s[9]]);
        let motion_sync = self.cmd(2, 1, 0x89, &[p])?[8] == 1;
        let angle_snap = self.cmd(2, 1, 0x84, &[p])?[8] == 1;
        Some(Settings { profile, stages, active, poll, lod, sleep_s, motion_sync, angle_snap })
    }

    // (процент, заряжается) — та же команда, что у иконки в трее
    pub fn battery(&self) -> Option<(u8, bool)> {
        let r = self.cmd(2, 0, 0x83, &[])?;
        (r[8] <= 100).then_some((r[8], r[7] != 0))
    }

    pub fn set_active(&self, p: u8, stage: usize) -> bool {
        self.cmd(2, 1, 0x02, &[p, stage as u8 + 1]).is_some()
    }

    // Все ступени разом: X и Y одинаковые, 4 байта на ступень
    pub fn set_stages(&self, p: u8, stages: &[u16]) -> bool {
        let mut d = vec![p, stages.len() as u8];
        for &v in stages {
            let v = clamp_dpi(v as i32).to_be_bytes();
            d.extend([v[0], v[1], v[0], v[1]]);
        }
        self.cmd(26, 1, 0x01, &d).is_some()
    }

    pub fn set_poll(&self, p: u8, hz: u16) -> bool { self.cmd(2, 1, 0x00, &[p, poll_code(hz)]).is_some() }
    pub fn set_lod(&self, p: u8, code: u8) -> bool { self.cmd(2, 1, 0x08, &[p, code]).is_some() }
    pub fn set_motion_sync(&self, p: u8, on: bool) -> bool { self.cmd(2, 1, 0x09, &[p, on as u8]).is_some() }
    pub fn set_angle_snap(&self, p: u8, on: bool) -> bool { self.cmd(2, 1, 0x04, &[p, on as u8]).is_some() }
    // Заводские настройки текущего профиля — кнопка «Сбросить профиль» драйвера (привязку донгла не трогает)
    pub fn reset_profile(&self, p: u8) -> bool { self.cmd(1, 0, 0x0D, &[p]).is_some() }
    pub fn set_sleep(&self, p: u8, secs: u16) -> bool {
        let b = secs.to_be_bytes();
        self.cmd(3, 0, 0x07, &[p, b[0], b[1]]).is_some()
    }
}

// DPI кратно 50, от 50 до максимума — как округляет драйвер
pub fn clamp_dpi(v: i32) -> u16 {
    ((v / DPI_STEP as i32) * DPI_STEP as i32).clamp(DPI_STEP as i32, DPI_MAX as i32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for &hz in POLL_DONGLE {
            assert_eq!(poll_hz(poll_code(hz)), hz);
        }
        assert_eq!(poll_hz(16), 1000);
        assert_eq!(clamp_dpi(1625), 1600);
        assert_eq!(clamp_dpi(0), 50);
        assert_eq!(clamp_dpi(99999), DPI_MAX);
    }

    // Только чтение с настоящей мыши: cargo test read_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn read_live() {
        for p in crate::hid_paths() {
            if let Some(d) = Dev::open(&p) {
                println!("{:?}", d.read());
            }
        }
    }
}
