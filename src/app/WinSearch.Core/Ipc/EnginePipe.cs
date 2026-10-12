using System.Security.Principal;

namespace WinSearch.Core.Ipc;

/// <summary>
/// Derives the engine pipe name. It embeds the user SID so two accounts on the
/// same machine never collide — matching ws-platform's naming on the Rust side
/// (which uses the full \\.\pipe\ path for CreateNamedPipeW).
/// </summary>
public static class EnginePipe
{
    /// <summary>Bare pipe name for NamedPipeClientStream (no \\.\pipe\ prefix).</summary>
    public static string NameForCurrentUser()
    {
        using var identity = WindowsIdentity.GetCurrent();
        var sid = identity.User?.Value
            ?? throw new InvalidOperationException("cannot determine current user SID");
        return $"winsearch-{sid}";
    }
}
