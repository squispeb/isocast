; Per-user, ordinary Win32 layout: the clients find the adjacent isocastd.exe.
; No service, PATH change, elevation, forced process exit or user-data cleanup.
#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef PayloadDir
  #error PayloadDir is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif

[Setup]
AppId=Isocast
AppName=Isocast
AppVersion={#AppVersion}
AppPublisher=Isocast
AppPublisherURL=https://github.com/R44VC0RP/isocast
DefaultDirName={localappdata}\Programs\Isocast
DefaultGroupName=Isocast
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19045
DisableProgramGroupPage=yes
DisableDirPage=yes
UsePreviousAppDir=yes
UninstallDisplayIcon={app}\isocast-ui.exe
OutputDir={#OutputDir}
OutputBaseFilename=Isocast-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
RestartIfNeededByRun=no
SetupLogging=yes
TimeStampsInUTC=yes

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion notimestamp

[Icons]
Name: "{userprograms}\Isocast"; Filename: "{app}\isocast-ui.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\isocast-ui.exe"; Description: "Open Isocast"; Flags: nowait postinstall skipifsilent

[Code]
function CreateFile(FileName: String; DesiredAccess, ShareMode: LongWord;
  SecurityAttributes: NativeUInt; CreationDisposition, FlagsAndAttributes: LongWord;
  TemplateFile: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): LongBool;
  external 'CloseHandle@kernel32.dll stdcall';

function FilesInUse(): Boolean;
var
  Found: TFindRec;
  Handle: THandle;
begin
  Result := False;
  if FindFirst(ExpandConstant('{app}\*'), Found) then begin
    try
      repeat
        if (CompareText(Found.Name, 'isocast.exe') = 0) or
           (CompareText(Found.Name, 'isocastd.exe') = 0) or
           (CompareText(Found.Name, 'isocast-ui.exe') = 0) or
           (CompareText(ExtractFileExt(Found.Name), '.dll') = 0) then begin
          { No writes: request an exclusive read/write handle, then close it.
            Windows denies write access to a running executable's image. }
          Handle := CreateFile(ExpandConstant('{app}\') + Found.Name,
            $C0000000, 0, 0, 3, $80, 0);
          if Handle = THandle(-1) then begin
            Result := True;
            Exit;
          end;
          CloseHandle(Handle);
        end;
      until not FindNext(Found);
    finally
      FindClose(Found);
    end;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if FilesInUse() then
    Result := 'Stop recording, streaming and exports, then quit Isocast and its engine before installing. Setup never stops a recording for you.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := not FilesInUse();
  if not Result then
    SuppressibleMsgBox('Stop recording, streaming and exports, then quit Isocast and its engine before uninstalling. Recordings and settings are kept.', mbError, MB_OK, IDOK);
end;

function InitializeSetup(): Boolean;
var
  Previous: String;
  OldVersion, NewVersion: Int64;
  Index: Integer;
begin
  Result := True;
  { Do not let command-line overrides enable Restart Manager termination. }
  for Index := 1 to ParamCount do begin
    if (CompareText(ParamStr(Index), '/CLOSEAPPLICATIONS') = 0) or
       (CompareText(ParamStr(Index), '/FORCECLOSEAPPLICATIONS') = 0) then begin
      SuppressibleMsgBox('Isocast Setup does not close applications. Stop your work and close Isocast yourself before installing.', mbError, MB_OK, IDOK);
      Result := False;
      Exit;
    end;
  end;
  if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Isocast_is1', 'DisplayVersion', Previous) and
     StrToVersion(Previous, OldVersion) and StrToVersion('{#AppVersion}', NewVersion) and
     (OldVersion > NewVersion) then begin
    Result := ExpandConstant('{param:ALLOWDOWNGRADE|0}') = '1';
    if not Result then
      SuppressibleMsgBox('A newer Isocast is installed. To roll back deliberately, close Isocast and run the previous trusted installer with /ALLOWDOWNGRADE=1. Your recordings and settings are kept.', mbError, MB_OK, IDOK);
  end;
end;
