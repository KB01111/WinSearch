using WinSearch.Core.Protocol;
using WinSearch.Core.Settings;
using WinSearch.ViewModels;
using Xunit;

namespace WinSearch.Tests;

public sealed class SettingsViewModelTests
{
    [Fact]
    public async Task ModelLifecycle_TransitionsAndActionStates_FollowStateMachine()
    {
        var fake = new FakeEngineClient();
        var now = new DateTimeOffset(2026, 10, 15, 12, 0, 0, TimeSpan.Zero);
        using var vm = new SettingsViewModel(new AppSettings(), fake, () => now);

        await vm.InitializeAsync();

        // Initial: NotDownloaded
        Assert.Equal(ModelState.NotDownloaded, vm.ModelState);
        Assert.True(vm.CanStartOrResumeDownload);
        Assert.False(vm.CanPauseDownload);
        Assert.True(vm.CanImportModel);
        Assert.False(vm.CanDeleteModel);
        Assert.Equal("Download", vm.PrimaryModelActionLabel);

        // Start download -> Downloading
        await vm.StartOrResumeDownloadAsync();
        Assert.Equal(1, fake.DownloadStartCalls);
        Assert.Equal(ModelState.Downloading, vm.ModelState);
        Assert.False(vm.CanStartOrResumeDownload);
        Assert.True(vm.CanPauseDownload);
        Assert.False(vm.CanImportModel);
        Assert.False(vm.CanDeleteModel);

        // Progress push 1 (at t=1s, 100 MiB of 800 MiB)
        now = now.AddSeconds(1);
        fake.PushModelStatus(new ModelStatus
        {
            State = ModelState.Downloading,
            Bytes = 100UL * 1024 * 1024,
            TotalBytes = 800UL * 1024 * 1024,
        });
        Assert.Equal(12.5, vm.ModelProgressPercent);
        Assert.Contains("100.0 MB / 800.0 MB", vm.ModelProgressText);
        Assert.Equal("100.0 MB/s", vm.DownloadSpeedText);

        // Progress push 2 (at t=3s, 300 MiB of 800 MiB -> 100 MB/s instant)
        now = now.AddSeconds(2);
        fake.PushModelStatus(new ModelStatus
        {
            State = ModelState.Downloading,
            Bytes = 300UL * 1024 * 1024,
            TotalBytes = 800UL * 1024 * 1024,
        });
        Assert.Equal(37.5, vm.ModelProgressPercent);
        Assert.Equal("100.0 MB/s", vm.DownloadSpeedText);

        // Pause -> Paused
        await vm.PauseDownloadAsync();
        Assert.Equal(1, fake.DownloadCancelCalls);
        Assert.Equal(ModelState.Paused, vm.ModelState);
        Assert.True(vm.CanStartOrResumeDownload);
        Assert.False(vm.CanPauseDownload);
        Assert.True(vm.CanImportModel);
        Assert.True(vm.CanDeleteModel);
        Assert.Equal("Resume", vm.PrimaryModelActionLabel);
        Assert.Equal("", vm.DownloadSpeedText);

        // Error state -> Retry + Delete enabled
        fake.PushModelStatus(new ModelStatus
        {
            State = ModelState.Error,
            Bytes = 300UL * 1024 * 1024,
            TotalBytes = 800UL * 1024 * 1024,
            Error = "SHA-256 mismatch",
        });
        Assert.Equal(ModelState.Error, vm.ModelState);
        Assert.Equal("Retry", vm.PrimaryModelActionLabel);
        Assert.Equal("SHA-256 mismatch", vm.ModelErrorText);
        Assert.True(vm.CanStartOrResumeDownload);
        Assert.True(vm.CanDeleteModel);

        // Import -> Ready
        await vm.ImportModelFromPathAsync(@"C:\models");
        Assert.Single(fake.ImportCalls);
        Assert.Equal(@"C:\models", fake.ImportCalls[0]);
        Assert.Equal(ModelState.Ready, vm.ModelState);
        Assert.Equal(100.0, vm.ModelProgressPercent);
        Assert.False(vm.CanStartOrResumeDownload);
        Assert.False(vm.CanPauseDownload);
        Assert.True(vm.CanImportModel);
        Assert.True(vm.CanDeleteModel);
        Assert.Equal("", vm.ModelErrorText);

        // Delete -> NotDownloaded
        await vm.DeleteModelAsync();
        Assert.Equal(1, fake.DeleteModelCalls);
        Assert.Equal(ModelState.NotDownloaded, vm.ModelState);
        Assert.Equal(0.0, vm.ModelProgressPercent);
        Assert.True(vm.CanStartOrResumeDownload);
        Assert.False(vm.CanDeleteModel);
    }

    [Fact]
    public async Task ConfigRoundTrip_SemanticToggleAndBackendSelection_UseIpcOnly()
    {
        var fake = new FakeEngineClient
        {
            CurrentConfig = new EngineConfig
            {
                SemanticEnabled = true,
                PreferredBackend = "vulkan",
            },
        };
        using var vm = new SettingsViewModel(new AppSettings(), fake);

        await vm.InitializeAsync();

        // Loaded from get_config without triggering a redundant set_config
        Assert.Empty(fake.SetConfigCalls);
        Assert.True(vm.EnableSemanticSearch);
        Assert.Equal("vulkan", vm.SelectedBackend);
        Assert.Contains("Vulkan", vm.DetectedBackendDisplay);

        // Toggling semantic search persists via set_config
        vm.EnableSemanticSearch = false;
        Assert.Single(fake.SetConfigCalls);
        Assert.False(fake.SetConfigCalls[0].SemanticEnabled);
        Assert.Equal("vulkan", fake.SetConfigCalls[0].PreferredBackend);

        // Changing backend persists via set_config and updates display text
        vm.SelectedBackend = "cuda";
        Assert.Equal(2, fake.SetConfigCalls.Count);
        Assert.Equal("cuda", fake.SetConfigCalls[1].PreferredBackend);
        Assert.Contains("CUDA", vm.DetectedBackendDisplay);
    }

    [Fact]
    public void AppSettings_ContainsOnlyUiPreferences()
    {
        // Verify engine-owned semantic/backend policy is not duplicated on AppSettings
        var props = typeof(AppSettings).GetProperties().Select(p => p.Name).ToHashSet();
        Assert.DoesNotContain("EnableSemanticSearch", props);
        Assert.DoesNotContain("SemanticSearchEnabled", props);
        Assert.DoesNotContain("PreferredBackend", props);
    }
}
