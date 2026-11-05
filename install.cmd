@echo off
REM Arranque para un Windows recien instalado.
REM
REM Existe porque un .ps1 no se puede ejecutar de entrada: Windows cliente viene
REM con la politica en Restricted, y ademas un fichero bajado de internet lleva
REM la marca MOTW, que lo bloquea aunque la politica lo permita. Las dos cosas
REM paran a alguien en su PRIMER comando, antes de haber instalado nada.
REM
REM Un .cmd no esta sujeto a la politica de ejecucion, y -ExecutionPolicy Bypass
REM cubre tambien el MOTW. Los argumentos se pasan tal cual, asi que
REM `install.cmd -Gpu` funciona igual que el script.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\install.ps1" %*
