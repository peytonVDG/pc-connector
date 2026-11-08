@echo off
rem Allows PC Connector through Windows Defender Firewall on private (home) networks.
rem Right-click this file and choose "Run as administrator".
setlocal
set "APP=%~dp0pc-connector.exe"
net session >nul 2>&1
if errorlevel 1 (
  echo Please right-click this file and choose "Run as administrator".
  pause
  exit /b 1
)
netsh advfirewall firewall delete rule name="PC Connector" >nul 2>&1
netsh advfirewall firewall add rule name="PC Connector" dir=in action=allow program="%APP%" enable=yes profile=private,domain
if errorlevel 1 (
  echo Failed to add the firewall rule.
) else (
  echo PC Connector is now allowed on private networks.
  echo Make sure your Wi-Fi network is set to "Private" in Settings ^> Network ^& internet.
)
pause
