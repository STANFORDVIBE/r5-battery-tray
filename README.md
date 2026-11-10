# R5BatteryTray

Маленькая иконка в трее Windows с зарядом мыши **Attack Shark R5 Ultra** (донгл 2.4G или провод). Rust + чистый Win32: ~1,5 МБ памяти, процессор не грузит.

- В трее батарейка, заполненная по заряду: зелёная — норм, красная — ≤20%, жёлтая — заряжается, пустая — мышь ещё ни разу не ответила (спит/выключена). Точный % — в подсказке при наведении.
- Обновляется раз в 30 секунд и по левому клику. Спящая мышь заряд не отдаёт — тогда остаётся последнее значение.
- Правый клик: «Обновить», «Запускать с Windows», «Выход».

## Запуск

Скачай `R5BatteryTray.exe` из [Releases](../../releases) и запусти. Установка не нужна.

## Сборка

Нужен [Rust](https://rustup.rs). `build.bat` соберёт и положит `R5BatteryTray.exe` рядом. Тесты: `cargo test`.

## Протокол

Feature-report 65 байт на интерфейс VID `373E` / PID `0047` (донгл) или `0046` (провод): запрос `00 00 00 02 02 00 83`, в ответе `[1]=A1`, `[7]` — зарядка, `[8]` — процент. Подсмотрено в [R5BatteryChecker](https://github.com/Xenofff/R5BatteryChecker), [r5-battery-estimator](https://github.com/lferraro1103/r5-battery-estimator) и [attackshark-battery-bridge](https://github.com/maxboeer/attackshark-battery-bridge).
