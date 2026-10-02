param([switch]$TrustedHost)
$ErrorActionPreference = 'Stop'
if (-not $TrustedHost) { throw 'Explicit -TrustedHost authorization is required.' }
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
try {
    $request = [Console]::In.ReadToEnd() | ConvertFrom-Json
    if (-not $request.action -or $request.timeout_seconds -lt 1 -or $request.timeout_seconds -gt 600) {
        throw 'Invalid native helper request.'
    }
    Add-Type -AssemblyName System.Drawing
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class AegisWindowsHost {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] struct Keyboard { public ushort Key, Scan; public uint Flags, Time; public UIntPtr Extra; }
    [StructLayout(LayoutKind.Sequential)] struct Mouse { public int X, Y; public uint Data, Flags, Time; public UIntPtr Extra; }
    [StructLayout(LayoutKind.Explicit)] struct InputUnion { [FieldOffset(0)] public Keyboard Keyboard; [FieldOffset(0)] public Mouse Mouse; }
    [StructLayout(LayoutKind.Sequential)] struct Input { public uint Type; public InputUnion Value; }
    [StructLayout(LayoutKind.Sequential)] struct JobBasic {
        public long ProcessTime, JobTime; public uint Flags; public UIntPtr Minimum, Maximum; public uint Active;
        public UIntPtr Affinity; public uint Priority, Scheduling;
    }
    [StructLayout(LayoutKind.Sequential)] struct IoCounters { public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes; }
    [StructLayout(LayoutKind.Sequential)] struct JobExtended {
        public JobBasic Basic; public IoCounters Io; public UIntPtr ProcessMemory, JobMemory, PeakProcess, PeakJob;
    }
    public class Window { public string handle, title; public uint process_id; public Rect rectangle; public bool foreground; }
    delegate bool EnumCallback(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumCallback callback, IntPtr parameter);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] static extern bool IsWindow(IntPtr window);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr window, StringBuilder text, int maximum);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr window, out Rect rectangle);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr window, int command);
    [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll", SetLastError=true)] static extern uint SendInput(uint count, Input[] input, int size);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job, int kind, ref JobExtended information, uint length);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr process, uint exitCode);
    static IntPtr ownedJob;
    static Timer watchdog;
    public static void BoundLifetime(int seconds, bool killChildren) {
        if (killChildren) {
            ownedJob = CreateJobObject(IntPtr.Zero, null);
            if (ownedJob == IntPtr.Zero) throw new Exception("Cannot create execution job");
            JobExtended limits = new JobExtended(); limits.Basic.Flags = 0x2000; // KILL_ON_JOB_CLOSE
            if (!SetInformationJobObject(ownedJob, 9, ref limits, (uint)Marshal.SizeOf(typeof(JobExtended))) ||
                !AssignProcessToJobObject(ownedJob, Process.GetCurrentProcess().Handle)) {
                CloseHandle(ownedJob); ownedJob = IntPtr.Zero;
                throw new Exception("Cannot contain PowerShell child lifetime; refusing execution");
            }
        }
        // Independent of the Node parent: a killed MCP client cannot leave an
        // indefinitely executing PowerShell child behind.
        // A hard process termination avoids PowerShell's ProcessExit handlers
        // waiting for the active pipeline. The OS closes our job handle and
        // terminates native descendants even when the MCP parent has died.
        watchdog = new Timer(delegate { TerminateProcess(GetCurrentProcess(), 124); }, null, seconds * 1000, Timeout.Infinite);
    }
    public static Window[] Windows() {
        List<Window> windows = new List<Window>(); IntPtr foreground = GetForegroundWindow();
        EnumWindows(delegate(IntPtr handle, IntPtr ignored) {
            if (!IsWindowVisible(handle)) return true;
            StringBuilder text = new StringBuilder(128); GetWindowText(handle, text, text.Capacity);
            if (text.Length == 0) return true;
            uint process; GetWindowThreadProcessId(handle, out process); Rect rectangle; GetWindowRect(handle, out rectangle);
            windows.Add(new Window { handle=handle.ToInt64().ToString(), title=text.ToString(), process_id=process,
                rectangle=rectangle, foreground=handle==foreground });
            return windows.Count < 200;
        }, IntPtr.Zero);
        return windows.ToArray();
    }
    public static void Focus(string value) {
        long number; if (!long.TryParse(value, out number) || number <= 0) throw new Exception("Invalid window handle");
        IntPtr window = new IntPtr(number);
        if (!IsWindow(window)) throw new Exception("Window no longer exists");
        ShowWindow(window, 9);
        if (GetForegroundWindow() == window) return;
        if (!SetForegroundWindow(window)) throw new Exception("Windows refused foreground activation");
        for (int attempt = 0; attempt < 50; attempt++) {
            if (GetForegroundWindow() == window) return;
            Thread.Sleep(10);
        }
        throw new Exception("Window did not become foreground");
    }
    static string QuoteArgument(string value) {
        StringBuilder quoted = new StringBuilder("\""); int slashes = 0;
        foreach (char unit in value) {
            if (unit == '\\') { slashes++; continue; }
            if (unit == '"') { quoted.Append('\\', slashes * 2 + 1); quoted.Append('"'); }
            else { quoted.Append('\\', slashes); quoted.Append(unit); }
            slashes = 0;
        }
        quoted.Append('\\', slashes * 2); quoted.Append('"'); return quoted.ToString();
    }
    public static int Launch(string executable, string[] arguments, string cwd, bool visible) {
        // ShellExecute starts an executable through Windows, not cmd/PowerShell
        // interpretation. It avoids inheriting Node's kill-on-close child job.
        ProcessStartInfo info = new ProcessStartInfo(executable);
        info.UseShellExecute = true; info.WorkingDirectory = cwd;
        info.WindowStyle = visible ? ProcessWindowStyle.Normal : ProcessWindowStyle.Hidden;
        List<string> quoted = new List<string>();
        foreach (string argument in arguments) quoted.Add(QuoteArgument(argument));
        info.Arguments = String.Join(" ", quoted.ToArray());
        using (Process child = Process.Start(info)) {
            if (child == null) throw new Exception("Windows did not return a launched process");
            return child.Id;
        }
    }
    static void Send(Input input) {
        if (SendInput(1, new Input[] { input }, Marshal.SizeOf(typeof(Input))) != 1)
            throw new Exception("Windows input injection failed (integrity level or desktop restrictions)");
    }
    public static void Pointer(int x, int y, string action, string button, int count, int delta) {
        if (!SetCursorPos(x, y)) throw new Exception("Cannot move cursor on this desktop");
        if (action == "move") return;
        if (action == "scroll") { Input wheel=new Input(); wheel.Value.Mouse.Flags=0x800; wheel.Value.Mouse.Data=unchecked((uint)delta); Send(wheel); return; }
        uint down=button=="right" ? 8u : button=="middle" ? 32u : 2u;
        for (int index=0; index<count; index++) {
            Input input=new Input(); input.Value.Mouse.Flags=down; Send(input);
            input.Value.Mouse.Flags=down*2; Send(input);
            if (index+1<count) Thread.Sleep(75);
        }
    }
    static ushort Key(string value) {
        string key=value.ToUpperInvariant();
        Dictionary<string, ushort> names=new Dictionary<string, ushort> {
            {"CTRL",17},{"SHIFT",16},{"ALT",18},{"WIN",91},{"ENTER",13},{"TAB",9},{"ESC",27},
            {"SPACE",32},{"BACKSPACE",8},{"DELETE",46},{"INSERT",45},{"LEFT",37},{"UP",38},{"RIGHT",39},
            {"DOWN",40},{"HOME",36},{"END",35},{"PAGEUP",33},{"PAGEDOWN",34}
        };
        ushort result; if (names.TryGetValue(key, out result)) return result;
        int function; if (key.Length>=2 && key[0]=='F' && int.TryParse(key.Substring(1), out function) && function>=1 && function<=24)
            return (ushort)(111+function);
        if (key.Length==1 && ((key[0]>='A' && key[0]<='Z') || (key[0]>='0' && key[0]<='9'))) return key[0];
        throw new Exception("Unsupported key: "+value);
    }
    static void KeyInput(ushort key, ushort scan, uint flags) {
        Input input=new Input(); input.Type=1; input.Value.Keyboard.Key=key;
        input.Value.Keyboard.Scan=scan; input.Value.Keyboard.Flags=flags; Send(input);
    }
    public static void Chord(string[] values) {
        ushort[] keys=new ushort[values.Length]; for (int index=0; index<values.Length; index++) keys[index]=Key(values[index]);
        int pressed=0;
        try { for (; pressed<keys.Length; pressed++) KeyInput(keys[pressed],0,0); }
        finally { for (int index=pressed-1; index>=0; index--) KeyInput(keys[index],0,2); }
    }
    public static void Text(string text) {
        foreach (char unit in text) { KeyInput(0,unit,4); KeyInput(0,unit,6); }
    }
}
'@
    [AegisWindowsHost]::BoundLifetime([int]$request.timeout_seconds, $request.action -eq 'powershell')
    $argsValue = $request.args
    switch ($request.action) {
        'app_launch' {
            $pidValue=[AegisWindowsHost]::Launch([string]$argsValue.executable,[string[]]@($argsValue.args),[string]$argsValue.cwd,[bool]$argsValue.visible)
            $result=@{ pid=$pidValue; visible=[bool]$argsValue.visible }
        }
        'powershell' {
            # Stream formatting instead of collecting arbitrarily large output.
            # The owning job terminates any spawned descendants when we exit.
            $global:LASTEXITCODE = 0
            & ([scriptblock]::Create([string]$argsValue.script)) | Out-String -Stream | ForEach-Object { [Console]::WriteLine($_) }
            exit $global:LASTEXITCODE
        }
        'desktop_windows' { $result = @{ windows=@([AegisWindowsHost]::Windows()) } }
        'desktop_focus' { [AegisWindowsHost]::Focus([string]$argsValue.handle); $result=@{ focused=$argsValue.handle } }
        'desktop_mouse' {
            $screen=[System.Windows.Forms.SystemInformation]::VirtualScreen
            if ($argsValue.x -lt $screen.Left -or $argsValue.x -ge $screen.Right -or $argsValue.y -lt $screen.Top -or $argsValue.y -ge $screen.Bottom) { throw 'Coordinates are outside the desktop.' }
            $button=if ($argsValue.button) { [string]$argsValue.button } else { 'left' }
            $count=if ($argsValue.count) { [int]$argsValue.count } else { 1 }
            [AegisWindowsHost]::Pointer([int]$argsValue.x,[int]$argsValue.y,[string]$argsValue.action,$button,$count,[int]$argsValue.delta)
            $result=@{ action=$argsValue.action; x=$argsValue.x; y=$argsValue.y }
        }
        'desktop_keyboard' {
            if ($null -ne $argsValue.text) { [AegisWindowsHost]::Text([string]$argsValue.text); $result=@{ typed_utf16_units=$argsValue.text.Length } }
            else { [AegisWindowsHost]::Chord([string[]]$argsValue.keys); $result=@{ keys=$argsValue.keys } }
        }
        'desktop_screenshot' {
            $screen=[System.Windows.Forms.SystemInformation]::VirtualScreen
            $region=if ($null -ne $argsValue.width) {
                [System.Drawing.Rectangle]::new([int]$argsValue.x,[int]$argsValue.y,[int]$argsValue.width,[int]$argsValue.height)
            } else { $screen }
            if ($region.Width -lt 1 -or $region.Height -lt 1 -or ([long]$region.Width*$region.Height) -gt 33554432 -or -not $screen.Contains($region)) { throw 'Invalid capture region for this desktop.' }
            $maxWidth=if ($argsValue.max_width) { [int]$argsValue.max_width } else { 1280 }
            $maxHeight=if ($argsValue.max_height) { [int]$argsValue.max_height } else { 1024 }
            $scale=[Math]::Min(1.0,[Math]::Min($maxWidth/[double]$region.Width,$maxHeight/[double]$region.Height))
            $width=[Math]::Max(1,[int]($region.Width*$scale)); $height=[Math]::Max(1,[int]($region.Height*$scale))
            $bitmap=[System.Drawing.Bitmap]::new($region.Width,$region.Height)
            $graphics=$null; $resized=$null; $resizeGraphics=$null
            try {
                $graphics=[System.Drawing.Graphics]::FromImage($bitmap)
                $graphics.CopyFromScreen($region.Location,[System.Drawing.Point]::Empty,$region.Size)
                $resized=[System.Drawing.Bitmap]::new($width,$height)
                $resizeGraphics=[System.Drawing.Graphics]::FromImage($resized)
                $resizeGraphics.InterpolationMode=[System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $resizeGraphics.DrawImage($bitmap,0,0,$width,$height)
                $resized.Save([string]$argsValue.output_path,[System.Drawing.Imaging.ImageFormat]::Png)
            } finally {
                if ($resizeGraphics) { $resizeGraphics.Dispose() }; if ($resized) { $resized.Dispose() }
                if ($graphics) { $graphics.Dispose() }; $bitmap.Dispose()
            }
            $result=@{ path=$argsValue.output_path; width=$width; height=$height;
                screen_region=@{ x=$region.X; y=$region.Y; width=$region.Width; height=$region.Height };
                screen_pixels_per_image_pixel=@{ x=$region.Width/[double]$width; y=$region.Height/[double]$height } }
        }
        default { throw 'Unsupported native action.' }
    }
    [Console]::WriteLine(($result | ConvertTo-Json -Depth 8 -Compress))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
