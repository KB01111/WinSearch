using System.Diagnostics;

namespace WinSearch.Core.Ipc;

/// <summary>
/// Locates and starts the engine process (<c>winsearchd.exe</c>). The engine
/// runs independently of the UI; this only ensures it exists.
/// </summary>
public static class EngineLocator
{
    /// <summary>
    /// Finds winsearchd.exe: explicit override, then the app-local engine
    /// directory, then the app directory itself (dev builds).
    /// </summary>
    public static string? FindEnginePath()
    {
        var overridePath = Environment.GetEnvironmentVariable("WINSEARCHD_PATH");
        if (!string.IsNullOrWhiteSpace(overridePath) && File.Exists(overridePath))
        {
            return overridePath;
        }

        var baseDir = AppContext.BaseDirectory;
        foreach (var candidate in new[]
                 {
                     Path.Combine(baseDir, "engine", "winsearchd.exe"),
                     Path.Combine(baseDir, "winsearchd.exe"),
                 })
        {
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }
        return null;
    }

    /// <summary>
    /// Starts the engine if it is not already running. Returns the process, or
    /// null when no engine binary could be found.
    /// </summary>
    public static Process? StartEngine()
    {
        var path = FindEnginePath();
        if (path is null)
        {
            return null;
        }

        var startInfo = new ProcessStartInfo(path)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            WorkingDirectory = Path.GetDirectoryName(path) ?? AppContext.BaseDirectory,
        };
        try
        {
            return Process.Start(startInfo);
        }
        catch
        {
            return null;
        }
    }
}
