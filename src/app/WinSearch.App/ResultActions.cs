using WinSearch.App.Interop;
using WinSearch.Core.Protocol;

namespace WinSearch.App;

/// <summary>Shell actions for search results: open, reveal, copy.</summary>
internal static class ResultActions
{
    /// <summary>
    /// Opens the result with its shell handler. Location-aware opening (PDF
    /// page, media timestamp, editor line) lands with the respective modality
    /// phases; until then the file itself opens.
    /// </summary>
    public static void Open(SearchResult result)
    {
        switch (result.Kind)
        {
            case ResultKind.Calculator:
                CopyText(result.Subtitle);
                return;
            case ResultKind.Settings:
                ShellOpen(result.Subtitle);
                return;
            case ResultKind.Action:
                // Action results carry their command in the path slot.
                if (!string.IsNullOrEmpty(result.Path))
                {
                    ShellOpen(result.Path);
                }
                return;
            default:
                var target = result.Path ?? result.Subtitle;
                if (!string.IsNullOrEmpty(target))
                {
                    ShellOpen(target);
                }
                return;
        }
    }

    public static void RevealInExplorer(SearchResult result)
    {
        if (result.Kind is ResultKind.Calculator or ResultKind.Settings or ResultKind.Action)
        {
            return;
        }

        var path = result.Path ?? result.Subtitle;
        if (string.IsNullOrWhiteSpace(path))
        {
            return;
        }

        string fullPath;
        try
        {
            fullPath = Path.GetFullPath(path);
        }
        catch
        {
            fullPath = path.Replace('/', '\\');
        }

        var hr = NativeMethods.SHParseDisplayName(fullPath, IntPtr.Zero, out var pidl, 0, out _);
        if (hr >= 0 && pidl != IntPtr.Zero)
        {
            try
            {
                // When cidl == 0, pidlFolder is the full item PIDL; Explorer opens its parent and selects the item.
                if (NativeMethods.SHOpenFolderAndSelectItems(pidl, 0, IntPtr.Zero, 0) >= 0)
                {
                    return;
                }
            }
            finally
            {
                NativeMethods.ILFree(pidl);
            }
        }

        if (File.Exists(fullPath) || Directory.Exists(fullPath))
        {
            NativeMethods.ShellExecuteW(IntPtr.Zero, "open", "explorer.exe", $"/select,\"{fullPath}\"", null, 1);
        }
    }

    public static void CopyPath(SearchResult result)
    {
        var path = result.Path ?? result.Subtitle;
        if (!string.IsNullOrEmpty(path))
        {
            CopyText(path);
        }
    }

    private static void ShellOpen(string target)
    {
        NativeMethods.ShellExecuteW(IntPtr.Zero, "open", target, null, null, 1);
    }

    private static void CopyText(string text)
    {
        var dataPackage = new Windows.ApplicationModel.DataTransfer.DataPackage();
        dataPackage.SetText(text);
        Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(dataPackage);
    }
}
