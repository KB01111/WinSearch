using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using WinSearch.Core.Settings;

namespace WinSearch.ViewModels;

/// <summary>Settings page state: theme, global shortcut, search behaviour.</summary>
public sealed partial class SettingsViewModel : ObservableObject
{
    private readonly AppSettings _settings;

    public SettingsViewModel(AppSettings settings)
    {
        _settings = settings;
        _selectedTheme = settings.Theme;
        _rememberLastQuery = settings.RememberLastQuery;
    }

    public static IReadOnlyList<AppTheme> ThemeOptions { get; } =
        Enum.GetValues<AppTheme>();

    [ObservableProperty]
    private AppTheme _selectedTheme;

    [ObservableProperty]
    private bool _rememberLastQuery;

    [ObservableProperty]
    private string _hotkeyDisplay = "";

    [ObservableProperty]
    private string _hotkeyStatus = "";

    public void Initialize()
    {
        HotkeyDisplay = _settings.HotkeyDisplay;
    }

    partial void OnSelectedThemeChanged(AppTheme value)
    {
        _settings.Theme = value;
        _settings.Save();
    }

    partial void OnRememberLastQueryChanged(bool value)
    {
        _settings.RememberLastQuery = value;
        _settings.Save();
    }

    /// <summary>
    /// Called by the view with a captured key chord. Returns true when the new
    /// hotkey was saved; the view is responsible for re-registering it and
    /// reporting conflicts via <see cref="HotkeyStatus"/>.
    /// </summary>
    public bool TrySetHotkey(uint modifiers, uint key)
    {
        _settings.HotkeyModifiers = modifiers;
        _settings.HotkeyKey = key;
        _settings.Save();
        HotkeyDisplay = _settings.HotkeyDisplay;
        HotkeyStatus = "";
        return true;
    }

    [RelayCommand]
    private void ResetHotkey()
    {
        TrySetHotkey(0x1, 0x20); // Alt + Space
    }
}
