using WinSearch.Core.Settings;

namespace WinSearch.App;

/// <summary>
/// Owns the global launcher hotkey (RegisterHotKey) and supports rebinding at
/// runtime with conflict detection.
/// </summary>
internal sealed class HotkeyManager : IDisposable
{
    private readonly Interop.MessageWindow _messageWindow;
    private int _currentId;

    public event Action? Pressed;

    public uint Modifiers { get; private set; }
    public uint Key { get; private set; }
    public bool IsRegistered => _currentId != 0;

    public HotkeyManager(Interop.MessageWindow messageWindow, AppSettings settings)
    {
        _messageWindow = messageWindow;
        Modifiers = settings.HotkeyModifiers;
        Key = settings.HotkeyKey;
    }

    /// <summary>Registers (or re-registers) the hotkey. False = chord taken.</summary>
    public bool TryRegister(uint modifiers, uint key)
    {
        Unregister();
        if (_messageWindow.TryRegisterHotkey(modifiers, key, OnPressed, out var id))
        {
            Modifiers = modifiers;
            Key = key;
            _currentId = id;
            return true;
        }
        return false;
    }

    /// <summary>Re-registers the previously configured chord.</summary>
    public bool TryRegisterCurrent() => TryRegister(Modifiers, Key);

    public void Unregister()
    {
        if (_currentId != 0)
        {
            _messageWindow.UnregisterHotkey(_currentId);
            _currentId = 0;
        }
    }

    private void OnPressed() => Pressed?.Invoke();

    public void Dispose() => Unregister();
}
