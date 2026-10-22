@echo off
rem Drag a .qbw file onto this file to open it in the viewer.
python "%~dp0qbw_viewer.py" %*
if errorlevel 1 pause
