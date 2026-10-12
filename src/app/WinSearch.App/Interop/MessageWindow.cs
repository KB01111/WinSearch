using System.Diagnostics;
using System.Runtime.InteropServices;

namespace WinSearch.App.Interop;

/// <summary>
/// A hidden Win32 message-only window that owns window-message traffic for the
/// global hotkey and the notification-area icon, independent of any WinUI
/// window's lifetime.
/// </summary>
internal sealed class MessageWindow : IDisposable
{
    private const string ClassName = "WinSearch.MessageWindow.v1";

    // Registered menu command ids for the tray menu.
    public const uint CmdOpen = 100;
    public const uint CmdSettings = 101;
    public const uint CmdPause = 102;
    public const uint CmdExit = 103;

    private static MessageWindow? _instance;
    private static NativeMethods.WndProcDelegate? _wndProcKeepAlive;

    private readonly Dictionary<int, Action> _hotkeyActions = new();
    private int _nextHotkeyId = 1;
    private bool _disposed;

    /// <summary>Raised when the tray icon is left- or right-clicked.</summary>
    public event Action? TrayActivated;

    /// <summary>Raised for tray menu commands (Cmd* constants).</summary>
    public event Action<uint>? MenuCommand;

    public IntPtr Handle { get; }

    private MessageWindow()
    {
        _wndProcKeepAlive = WndProc;
        var wc = new NativeMethods.WNDCLASSEXW
        {
            cbSize = (uint)Marshal.SizeOf<NativeMethods.WNDCLASSEXW>(),
            lpfnWndProc = _wndProcKeepAlive,
            hInstance = NativeMethods.GetModuleHandleW(null),
            lpszClassName = ClassName,
        };
        var atom = NativeMethods.RegisterClassExW(ref wc);
        Debug.Assert(atom != 0, "RegisterClassExW failed");

        Handle = NativeMethods.CreateWindowExW(
            0, ClassName, "WinSearch Message Window",
            0, 0, 0, 0, 0,
            NativeMethods.HWND_MESSAGE, IntPtr.Zero, wc.hInstance, IntPtr.Zero);
        Debug.Assert(Handle != IntPtr.Zero, "CreateWindowExW failed");
    }

    /// <summary>The window is created lazily and shared process-wide.</summary>
    public static MessageWindow Instance => _instance ??= new MessageWindow();

    private static IntPtr WndProc(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam)
    {
        var self = _instance;
        if (self is not null && !self._disposed)
        {
            switch (msg)
            {
                case NativeMethods.WM_HOTKEY:
                    if (self._hotkeyActions.TryGetValue(wParam.ToInt32(), out var action))
                    {
                        action();
                    }
                    return IntPtr.Zero;

                case NativeMethods.WM_TRAYICON:
                    if (lParam.ToInt32() is (int)NativeMethods.WM_LBUTTONUP
                        or (int)NativeMethods.WM_RBUTTONUP)
                    {
                        self.TrayActivated?.Invoke();
                    }
                    return IntPtr.Zero;

                case NativeMethods.WM_COMMAND:
                    self.MenuCommand?.Invoke((uint)(wParam.ToInt32() & 0xFFFF));
                    return IntPtr.Zero;
            }
        }
        return NativeMethods.DefWindowProcW(hWnd, msg, wParam, lParam);
    }

    /// <summary>
    /// Registers a system-wide hotkey delivering to <paramref name="action"/>.
    /// Returns false when the chord is already taken by another application.
    /// </summary>
    public bool TryRegisterHotkey(uint modifiers, uint key, Action action, out int id)
    {
        id = _nextHotkeyId;
        // MOD_NOREPEAT keeps one press = one invocation on key auto-repeat.
        if (!NativeMethods.RegisterHotKey(Handle, id, modifiers | NativeMethods.MOD_NOREPEAT, key))
        {
            id = 0;
            return false;
        }
        _hotkeyActions[id] = action;
        _nextHotkeyId++;
        return true;
    }

    public void UnregisterHotkey(int id)
    {
        if (id != 0 && _hotkeyActions.Remove(id))
        {
            NativeMethods.UnregisterHotKey(Handle, id);
        }
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }
        _disposed = true;
        foreach (var id in _hotkeyActions.Keys.ToArray())
        {
            NativeMethods.UnregisterHotKey(Handle, id);
        }
        _hotkeyActions.Clear();
        if (Handle != IntPtr.Zero)
        {
            NativeMethods.DestroyWindow(Handle);
        }
    }
}
