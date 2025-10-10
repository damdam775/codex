@echo off
setlocal EnableDelayedExpansion
set "SCRIPT_DIR=%~dp0"
pushd "%SCRIPT_DIR%"

if not exist codex-cli (
    echo codex-cli directory not found. Ensure you are running install.bat from the repository root.
    popd
    exit /b 1
)

pushd codex-cli

call :run corepack enable
if errorlevel 1 goto :fail
call :run pnpm install
if errorlevel 1 goto :fail
call :run pnpm build
if errorlevel 1 goto :fail

if exist scripts\install_native_deps.sh (
    echo Skipping scripts\install_native_deps.sh (Linux-only step).
)

call :run node .\dist\cli.js --help
if errorlevel 1 goto :fail
call :run node .\dist\cli.js
if errorlevel 1 goto :fail
call :run pnpm link
if errorlevel 1 goto :fail

popd
popd
exit /b 0

:fail
popd
popd
exit /b 1

:run
echo.
echo ===^> %*
%*
if errorlevel 1 (
    echo Command failed: %*
    exit /b 1
)
exit /b 0
