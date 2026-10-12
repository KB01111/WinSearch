using System.Runtime.InteropServices;

namespace WinSearch.App;

/// <summary>
/// Notification-area icon with an Open / Settings / Exit menu, matching the
/// "lives in the menu bar" role DigUp plays on macOS.
/// </summary>
internal sealed class TrayIcon : IDisposable
{
    private readonly Interop.MessageWindow _messageWindow;
    private Interop.NativeMethods.NOTIFYICONDATAW _data;
    private readonly IntPtr _icon;
    private bool _visible;

    public event Action? OpenRequested;
    public event Action? SettingsRequested;
    public event Action? ExitRequested;

    public TrayIcon(Interop.MessageWindow messageWindow, string tooltip)
    {
        _messageWindow = messageWindow;
        _messageWindow.TrayActivated += ShowMenu;

        var iconPath = Path.Combine(AppContext.BaseDirectory, "Assets", "WinSearch.ico");
        _icon = Interop.NativeMethods.LoadImageW(
            IntPtr.Zero, iconPath,
            Interop.NativeMethods.IMAGE_ICON, 0, 0,
            Interop.NativeMethods.LR_LOADFROMFILE | Interop.NativeMethods.LR_DEFAULTSIZE);

        _data = new Interop.NativeMethods.NOTIFYICONDATAW
        {
            cbSize = (uint)Marshal.SizeOf<Interop.NativeMethods.NOTIFYICONDATAW>(),
            hWnd = messageWindow.Handle,
            uID = 1,
            uFlags = Interop.NativeMethods.NIF_MESSAGE | Interop.NativeMethods.NIF_ICON | Interop.NativeMethods.NIF_TIP,
            uCallbackMessage = Interop.NativeMethods.WM_TRAYICON,
            hIcon = _icon,
            szTip = tooltip,
            szInfo = "",
            szInfoTitle = "",
        };
    }

    public void Show()
    {
        if (_visible)
        {
            return;
        }
        Interop.NativeMethods.Shell_NotifyIconW(Interop.NativeMethods.NIM_ADD, ref _data);
        _visible = true;
    }

    public void UpdateTooltip(string tooltip)
    {
        if (!_visible)
        {
            return;
        }
        _data.szTip = tooltip;
        Interop.NativeMethods.Shell_NotifyIconW(Interop.NativeMethods.NIM_MODIFY, ref _data);
    }

    private void ShowMenu()
    {
        var menu = Interop.NativeMethods.CreatePopupMenu();
        if (menu == IntPtr.Zero)
        {
            return;
        }

        Interop.NativeMethods.AppendMenuW(menu, Interop.NativeMethods.MF_STRING, Interop.MessageWindow.CmdOpen, "Open WinSearch");
        Interop.NativeMethods.AppendMenuW(menu, Interop.NativeMethods.MF_STRING, Interop.MessageWindow.CmdSettings, "Settings…");
        Interop.NativeMethods.AppendMenuW(menu, Interop.NativeMethods.MF_SEPARATOR, UIntPtr.Zero, null);
        Interop.NativeMethods.AppendMenuW(menu, Interop.NativeMethods.MF_STRING, Interop.MessageWindow.CmdExit, "Exit");

        Interop.NativeMethods.GetCursorPos(out var point);
        // Required so the menu dismisses correctly when clicking elsewhere.
        Interop.NativeMethods.SetForegroundWindow(_messageWindow.Handle);
        var cmd = Interop.NativeMethods.TrackPopupMenu(
            menu,
            Interop.NativeMethods.TPM_RIGHTBUTTON | Interop.NativeMethods.TPM_RETURNCMD,
            point.X, point.Y, 0, _messageWindow.Handle, IntPtr.Zero);
        Interop.NativeMethods.DestroyMenu(menu);

        switch (cmd)
        {
            case Interop.MessageWindow.CmdOpen:
                OpenRequested?.Invoke();
                break;
            case Interop.MessageWindow.CmdSettings:
                SettingsRequested?.Invoke();
                break;
            case Interop.MessageWindow.CmdExit:
                ExitRequested?.Invoke();
                break;
        }
    }

    public void Dispose()
    {
        if (_visible)
        {
            Interop.NativeMethods.Shell_NotifyIconW(Interop.NativeMethods.NIM_DELETE, ref _data);
            _visible = false;
        }
        if (_icon != IntPtr.Zero)
        {
            Interop.NativeMethods.DestroyIcon(_icon);
        }
        _messageWindow.TrayActivated -= ShowMenu;
    }
}
