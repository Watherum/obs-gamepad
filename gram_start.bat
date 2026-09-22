@echo off
rem Usage: gram_start.bat [port] [extra flags...]
rem   gram_start.bat            -> port 8080
rem   gram_start.bat 9000       -> port 9000
rem   gram_start.bat --port 9000
set "PORTARG="
echo %~1| findstr /r "^[0-9][0-9]*$" >nul && (set "PORTARG=--port %~1" & shift)
cargo run -- --web --scale 3 %PORTARG% %1 %2 %3 %4 %5 %6 %7 %8 %9 layouts/gram.toml
pause
