using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using WinSearch.App.Interop;

namespace WinSearch.App;

/// <summary>
/// Monitor-aware placement and chrome removal for the overlay window: it opens
/// centered on the monitor that contains the foreground window, scaled for that
/// monitor's DPI, with rounded borderless corners.
/// </summary>
internal static class OverlayPlacement
{
    /// <summary>Design width at 100% scaling (spec: ~760 px).</summary>
    private const double DesignWidth = 760;

    /// <summary>Initial height; results area adapts as content arrives.</summary>
    private const double DesignHeight = 520;

    /// <summary>Relative vertical position inside the monitor work area.</summary>
    private const double TopFraction = 0.20;

    public static void ApplyChrome(Window window)
    {
        var hwnd = WindowNativeHandle(window);
        if (hwnd == IntPtr.Zero)
        {
            return;
        }

        // Borderless: no caption, no resize border; content paints edge to edge.
        if (window.AppWindow?.Presenter is OverlappedPresenter presenter)
        {
            presenter.SetBorderAndTitleBar(false, false);
            presenter.IsResizable = false;
            presenter.IsMaximizable = false;
            presenter.IsMinimizable = false;
        }

        // Keep the overlay out of Alt-Tab and the taskbar like a true popup.
        var exStyle = NativeMethods.GetWindowLongPtrW(hwnd, NativeMethods.GWL_EXSTYLE);
        NativeMethods.SetWindowLongPtrW(
            hwnd, NativeMethods.GWL_EXSTYLE,
            new IntPtr(exStyle.ToInt64() | NativeMethods.WS_EX_TOOLWINDOW));

        // Rounded corners on Windows 11.
        var corner = NativeMethods.DWMWCP_ROUND;
        NativeMethods.DwmSetWindowAttribute(
            hwnd, NativeMethods.DWMWA_WINDOW_CORNER_PREFERENCE, ref corner, sizeof(int));
    }

    public static void PlaceOnActiveMonitor(Window window)
    {
        var hwnd = WindowNativeHandle(window);
        if (hwnd == IntPtr.Zero || window.AppWindow is null)
        {
            return;
        }

        // The monitor of whichever window had focus when the hotkey fired.
        var foreground = NativeMethods.GetForegroundWindow();
        var monitor = NativeMethods.MonitorFromWindow(foreground, NativeMethods.MONITOR_DEFAULTTONEAREST);

        var info = new NativeMethods.MONITORINFO
        {
            cbSize = (uint)System.Runtime.InteropServices.Marshal.SizeOf<NativeMethods.MONITORINFO>(),
        };
        if (!NativeMethods.GetMonitorInfoW(monitor, ref info))
        {
            return;
        }

        uint dpi = 96;
        try
        {
            dpi = NativeMethods.GetDpiForWindow(hwnd);
        }
        catch
        {
            // GetDpiForWindow requires Windows 10 1607+; fall back to 96 DPI.
        }
        if (dpi == 0)
        {
            dpi = 96;
        }

        var scale = dpi / 96.0;
        var width = (int)(DesignWidth * scale);
        var height = (int)(DesignHeight * scale);

        var work = info.rcWork;
        var x = work.Left + (work.Width - width) / 2;
        var y = work.Top + (int)(work.Height * TopFraction);

        window.AppWindow.MoveAndResize(new Windows.Graphics.RectInt32(x, y, width, height));
    }

    public static IntPtr WindowNativeHandle(Window window)
    {
        var windowNative = WinRT.Interop.WindowNative.GetWindowHandle(window);
        return windowNative;
    }
}
