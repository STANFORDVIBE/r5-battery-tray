@echo off
rem Builds the Rust version and copies R5BatteryTray.exe next to this file (autostart points here).
cd /d "%~dp0"
cargo build --release || exit /b 1
taskkill /im R5BatteryTray.exe /f >nul 2>&1
copy /y target\release\R5BatteryTray.exe R5BatteryTray.exe >nul
