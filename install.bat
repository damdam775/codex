@echo off
setlocal EnableDelayedExpansion
set "SCRIPT_DIR=%~dp0"
pushd "%SCRIPT_DIR%"

if not exist codex-cli (
    echo codex-cli directory not found. Ensure you are running install.bat from the repository root.
    goto :fail_root
)

pushd codex-cli

call :run corepack enable
if errorlevel 1 goto :fail_cli
call :run pnpm install
if errorlevel 1 goto :fail_cli
call :run pnpm build
if errorlevel 1 goto :fail_cli

if exist scripts\install_native_deps.sh (
    echo Skipping scripts\install_native_deps.sh (Linux-only step).
)

call :run node .\dist\cli.js --help
if errorlevel 1 goto :fail_cli
call :run node .\dist\cli.js
if errorlevel 1 goto :fail_cli
call :run pnpm link
if errorlevel 1 goto :fail_cli

popd

if not exist codex-rs (
    echo codex-rs directory not found. Ensure you are running install.bat from the repository root.
    goto :fail_root
)

pushd codex-rs

call :run cargo install --locked --path cli
if errorlevel 1 goto :fail_rs

set "CODEX_BIN_PATH="
if defined CARGO_HOME (
    if exist "%CARGO_HOME%\bin\codex.exe" set "CODEX_BIN_PATH=%CARGO_HOME%\bin\codex.exe"
    if not defined CODEX_BIN_PATH if exist "%CARGO_HOME%\bin\codex" set "CODEX_BIN_PATH=%CARGO_HOME%\bin\codex"
)
if not defined CODEX_BIN_PATH if exist "%USERPROFILE%\.cargo\bin\codex.exe" set "CODEX_BIN_PATH=%USERPROFILE%\.cargo\bin\codex.exe"
if not defined CODEX_BIN_PATH if exist "%USERPROFILE%\.cargo\bin\codex" set "CODEX_BIN_PATH=%USERPROFILE%\.cargo\bin\codex"

if defined CODEX_BIN_PATH (
    call :run "%CODEX_BIN_PATH%" --version
    if errorlevel 1 goto :fail_rs
    call :run "%CODEX_BIN_PATH%" --help
    if errorlevel 1 goto :fail_rs
) else (
    call :run codex --version
    if errorlevel 1 goto :fail_rs
    call :run codex --help
    if errorlevel 1 goto :fail_rs
)

popd
popd
exit /b 0

:fail_rs
popd
:fail_root
popd
exit /b 1

:fail_cli
popd
goto :fail_root

:run
echo.
echo ===^> %*
%*
if errorlevel 1 (
    echo Command failed: %*
    exit /b 1
)
exit /b 0
