using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;
using WinSearch.Core.Settings;
using WinSearch.ViewModels;

namespace WinSearch.App;

/// <summary>
/// Settings window. Phase 1 covers General (theme, shortcut, search behaviour);
/// additional sections arrive with their feature phases.
/// </summary>
internal sealed partial class SettingsWindow : Window
{
    private readonly AppSettings _settings;
    private readonly HotkeyManager _hotkeyManager;
    private readonly SettingsViewModel _viewModel;
    private bool _capturingHotkey;

    public SettingsWindow(AppSettings settings, HotkeyManager hotkeyManager)
    {
        _settings = settings;
        _hotkeyManager = hotkeyManager;
        _viewModel = new SettingsViewModel(settings);
        InitializeComponent();

        Title = "WinSearch Settings";
        SystemBackdrop = new Microsoft.UI.Xaml.Media.DesktopAcrylicBackdrop();
        RootGrid.RequestedTheme = _settings.Theme switch
        {
            AppTheme.Light => ElementTheme.Light,
            AppTheme.Dark => ElementTheme.Dark,
            _ => ElementTheme.Default,
        };

        _viewModel.Initialize();
        HotkeyText.Text = _viewModel.HotkeyDisplay;

        ThemeCombo.ItemsSource = SettingsViewModel.ThemeOptions;
        ThemeCombo.SelectedItem = _viewModel.SelectedTheme;
        RememberQueryToggle.IsOn = _viewModel.RememberLastQuery;

        RootGrid.KeyDown += OnRootKeyDown;
    }

    private void OnThemeChanged(object sender, SelectionChangedEventArgs e)
    {
        if (ThemeCombo.SelectedItem is AppTheme theme)
        {
            _viewModel.SelectedTheme = theme;
            RootGrid.RequestedTheme = theme switch
            {
                AppTheme.Light => ElementTheme.Light,
                AppTheme.Dark => ElementTheme.Dark,
                _ => ElementTheme.Default,
            };
        }
    }

    private void OnChangeHotkeyClick(object sender, RoutedEventArgs e)
    {
        _capturingHotkey = true;
        ChangeHotkeyButton.Content = "Press keys…";
        HotkeyStatusText.Text = "Press the new combination (Esc to cancel).";
    }

    private void OnResetHotkeyClick(object sender, RoutedEventArgs e)
    {
        _capturingHotkey = false;
        ChangeHotkeyButton.Content = "Change";
        ApplyHotkey(0x1, 0x20); // Alt + Space
    }

    private void OnRootKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (!_capturingHotkey)
        {
            return;
        }
        e.Handled = true;

        if (e.Key == VirtualKey.Escape)
        {
            _capturingHotkey = false;
            ChangeHotkeyButton.Content = "Change";
            HotkeyStatusText.Text = "Capture cancelled.";
            return;
        }

        uint modifiers = 0;
        var keyboard = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread;
        if ((keyboard(VirtualKey.LeftWindows) & Windows.UI.Core.CoreVirtualKeyStates.Down) != 0
            || (keyboard(VirtualKey.RightWindows) & Windows.UI.Core.CoreVirtualKeyStates.Down) != 0)
        {
            modifiers |= NativeMethods_MOD_WIN;
        }
        if ((keyboard(VirtualKey.Control) & Windows.UI.Core.CoreVirtualKeyStates.Down) != 0)
        {
            modifiers |= NativeMethods_MOD_CONTROL;
        }
        if ((keyboard(VirtualKey.Menu) & Windows.UI.Core.CoreVirtualKeyStates.Down) != 0)
        {
            modifiers |= NativeMethods_MOD_ALT;
        }
        if ((keyboard(VirtualKey.Shift) & Windows.UI.Core.CoreVirtualKeyStates.Down) != 0)
        {
            modifiers |= NativeMethods_MOD_SHIFT;
        }

        // Modifier-only presses are not a valid chord yet.
        if (e.Key is VirtualKey.Control or VirtualKey.Menu or VirtualKey.Shift
            or VirtualKey.LeftWindows or VirtualKey.RightWindows)
        {
            return;
        }

        _capturingHotkey = false;
        ChangeHotkeyButton.Content = "Change";
        ApplyHotkey(modifiers, (uint)e.Key);
    }

    private void ApplyHotkey(uint modifiers, uint key)
    {
        if (_hotkeyManager.TryRegister(modifiers, key))
        {
            _viewModel.TrySetHotkey(modifiers, key);
            HotkeyText.Text = _viewModel.HotkeyDisplay;
            HotkeyStatusText.Text = "Shortcut active.";
        }
        else
        {
            // Re-register the previous chord so the launcher keeps working.
            _hotkeyManager.TryRegisterCurrent();
            HotkeyStatusText.Text =
                $"{AppSettings.FormatHotkey(modifiers, key)} is already used by another application. Choose a different combination.";
        }
    }

    private void OnRememberQueryToggled(object sender, RoutedEventArgs e)
    {
        _viewModel.RememberLastQuery = RememberQueryToggle.IsOn;
    }

    private const uint NativeMethods_MOD_ALT = 0x1;
    private const uint NativeMethods_MOD_CONTROL = 0x2;
    private const uint NativeMethods_MOD_SHIFT = 0x4;
    private const uint NativeMethods_MOD_WIN = 0x8;
}
