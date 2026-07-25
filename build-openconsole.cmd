@echo off
setlocal
set "VCPKG_ROOT=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\vcpkg"
set "OPENCON=G:\Programming\Repos\microsoft-terminal"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=amd64 -host_arch=amd64
if errorlevel 1 exit /b %errorlevel%
"%OPENCON%\dep\nuget\nuget.exe" restore "%OPENCON%\build\packages.config" -ConfigFile "%OPENCON%\NuGet.config" -Verbosity quiet -PackagesDirectory "%OPENCON%\packages"
if errorlevel 1 exit /b %errorlevel%
"%OPENCON%\dep\nuget\nuget.exe" restore "%OPENCON%\dep\nuget\packages.config" -ConfigFile "%OPENCON%\NuGet.config" -Verbosity quiet -PackagesDirectory "%OPENCON%\packages"
if errorlevel 1 exit /b %errorlevel%
msbuild "%OPENCON%\OpenConsole.slnx" /t:Restore /p:Configuration=Release /p:Platform=x64 /p:SolutionDir=%OPENCON%\ /p:OpenConsoleDir=%OPENCON%\ /m /nologo
if errorlevel 1 exit /b %errorlevel%
msbuild "%OPENCON%\src\host\exe\Host.EXE.vcxproj" /p:Configuration=Release /p:Platform=x64 /p:SolutionDir=%OPENCON%\ /p:OpenConsoleDir=%OPENCON%\ /m /nologo
if errorlevel 1 exit /b %errorlevel%
msbuild "%OPENCON%\src\winconpty\dll\winconptydll.vcxproj" /p:Configuration=Release /p:Platform=x64 /p:SolutionDir=%OPENCON%\ /p:OpenConsoleDir=%OPENCON%\ /m /nologo
exit /b %errorlevel%
