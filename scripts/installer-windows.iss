; Synly Windows 安装器脚本 (Inno Setup 6).
;
; 由 scripts/package-windows.ps1 调用 iscc 编译, 可变项全部通过 /D 传入:
;   AppVersion      应用版本, 显示在卸载项的 DisplayVersion
;   AppArch         目标架构 (x86_64 或 aarch64)
;   PayloadDir      载荷目录, 含 synly.exe, 随附 dll 与 audio-licenses
;   OutputDir       安装器输出目录
;   OutputBaseName  安装器文件名 (不含扩展名)
;
; AppId 一经发布不得更改: 它决定卸载项名与升级是否追加到同一份卸载日志.
; 安装目录只放程序自有文件, 用户数据 (配置, 日志, 更新缓存) 一律不放这里,
; 升级与卸载都不会触碰用户数据目录.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef AppArch
  #define AppArch "x86_64"
#endif
#ifndef PayloadDir
  #define PayloadDir "payload"
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef OutputBaseName
  #define OutputBaseName "synly-setup"
#endif

#define AppName "Synly"
#define AppExeName "synly.exe"
#define AppPublisher "azazo1"
#define AppUrl "https://github.com/azazo1/synly"

#if AppArch == "aarch64"
  #define ArchAllowed "arm64"
#else
  #define ArchAllowed "x64compatible"
#endif

[Setup]
AppId={{B354AB28-E96A-4AF4-9988-253DA25F421F}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
DefaultDirName={userpf}\{#AppName}
DefaultGroupName={#AppName}
UsePreviousAppDir=yes
DisableDirPage=auto
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseName}
SetupIconFile=..\assets\windows\synly.ico
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#ArchAllowed}
ArchitecturesInstallIn64BitMode={#ArchAllowed}
MinVersion=10.0
; 应用是托盘驻留程序, 关闭窗口只隐藏到托盘, 不会响应 Restart Manager 的关闭请求,
; 让 RM 兜底只会让静默升级在应用尚未退出时中止 (Abort), 而占用文件其实由 [Code] 先行
; 让位解决. 重新拉起应用交给 [Run] 的 postinstall 项, 也不需要 Restart Manager 代劳.
CloseApplications=no
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\*.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#PayloadDir}\audio-licenses\*"; DestDir: "{app}\audio-licenses"; Flags: ignoreversion skipifsourcedoesntexist

[InstallDelete]
; 音频许可目录先整目录清掉再重新复制, 上一版本有而本版本不再提供的许可文件随之消失.
Type: filesandordirs; Name: "{app}\audio-licenses"

[UninstallDelete]
Type: files; Name: "{app}\*.old"
Type: filesandordirs; Name: "{app}\audio-licenses"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
; 静默安装也会执行: 应用把落地工作交接给安装器后自己退出, 由这里把新版重新拉起.
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall

[Code]
const
  DisplacedSuffix = '.old';

var
  InstallFinalized: Boolean;

function IsManagedFileName(const Name: String): Boolean;
var
  Extension: String;
  Lower: String;
begin
  Lower := Lowercase(Name);
  Extension := Lowercase(ExtractFileExt(Name));
  if (Extension <> '.exe') and (Extension <> '.dll') then
  begin
    Result := False;
    Exit;
  end;
  { Inno 自己的卸载器由安装器维护, 不能当成旧程序文件让位或删除. }
  Result := Copy(Lower, 1, 5) <> 'unins';
end;

{ 覆盖程序文件之前先处理被占用的旧文件.
  运行中的进程 (主程序自身或 SYSTEM 输入服务) 会映射着旧映像, 直接覆盖会失败;
  Windows 允许把运行中的映像改名, 因此让位成 <name>.old 后再写入新文件.
  让位文件由应用下次启动时清理, 服务重启后即可删除. }
procedure MoveDisplacedFilesAside;
var
  FindRec: TFindRec;
  Root: String;
  Target: String;
  Backup: String;
  Attempt: Integer;
begin
  Root := ExpandConstant('{app}');
  if not DirExists(Root) then
    Exit;
  if not FindFirst(AddBackslash(Root) + '*', FindRec) then
    Exit;
  try
    repeat
      if (FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then
        Continue;
      if not IsManagedFileName(FindRec.Name) then
        Continue;
      Target := AddBackslash(Root) + FindRec.Name;
      if DeleteFile(Target) then
      begin
        Log('已删除旧程序文件: ' + Target);
        Continue;
      end;
      Backup := Target + DisplacedSuffix;
      Attempt := 0;
      while FileExists(Backup) and (Attempt < 1000) do
      begin
        Attempt := Attempt + 1;
        Backup := Target + DisplacedSuffix + '.' + IntToStr(Attempt);
      end;
      if RenameFile(Target, Backup) then
        Log('旧程序文件仍被占用, 已改名让位: ' + Backup)
      else
        Log('无法让位旧程序文件, 需要先关闭占用它的进程: ' + Target);
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

{ 安装没有走完时把让位的旧程序文件放回去, 避免留下一个主程序缺失的安装目录. }
procedure RestoreDisplacedFiles;
var
  FindRec: TFindRec;
  Root: String;
  Source: String;
  Target: String;
  Marker: Integer;
begin
  Root := ExpandConstant('{app}');
  if not DirExists(Root) then
    Exit;
  if not FindFirst(AddBackslash(Root) + '*' + DisplacedSuffix + '*', FindRec) then
    Exit;
  try
    repeat
      if (FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then
        Continue;
      Marker := Pos(DisplacedSuffix, FindRec.Name);
      if Marker <= 1 then
        Continue;
      Source := AddBackslash(Root) + FindRec.Name;
      Target := AddBackslash(Root) + Copy(FindRec.Name, 1, Marker - 1);
      if FileExists(Target) then
        Continue;
      if RenameFile(Source, Target) then
        Log('安装未完成, 已恢复旧程序文件: ' + Target)
      else
        Log('安装未完成, 但无法恢复旧程序文件: ' + Source);
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    MoveDisplacedFilesAside
  else if CurStep = ssPostInstall then
    InstallFinalized := True;
end;

procedure DeinitializeSetup();
begin
  if not InstallFinalized then
    RestoreDisplacedFiles;
end;
