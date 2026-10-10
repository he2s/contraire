@echo off
rem Install contraire from cmd.exe: runs install.ps1 with PowerShell.
rem   curl.exe -fsSLO https://raw.githubusercontent.com/he2s/contraire/main/install.cmd && install.cmd
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/he2s/contraire/main/install.ps1 | iex"
