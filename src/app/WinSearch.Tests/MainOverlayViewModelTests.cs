using System.Runtime.CompilerServices;
using System.Threading.Channels;
using WinSearch.Core.Protocol;
using WinSearch.Core.Settings;
using WinSearch.ViewModels;
using Xunit;

namespace WinSearch.Tests;

public sealed class MainOverlayViewModelTests
{
    [Fact]
    public async Task SemanticStreaming_PreservesExactOrder_AppendsSemantic_AndTracksIsSemanticPending()
    {
        var channel = Channel.CreateUnbounded<SearchBatch>();
        var fake = new FakeEngineClient
        {
            CurrentConfig = new EngineConfig { SemanticEnabled = true, PreferredBackend = "auto" },
            CurrentModelStatus = new ModelStatus { State = ModelState.Ready },
            SearchHandler = (_, ct) => ReadBatchesAsync(channel.Reader, ct),
        };

        await using var vm = new MainOverlayViewModel(fake, new AppSettings());
        await vm.InitializeAsync();

        var searchTask = vm.RunSearchAsync("meeting");

        // Emit exact batch (Done = false because semantic stage follows)
        channel.Writer.TryWrite(new SearchBatch
        {
            Results =
            [
                new SearchResult
                {
                    Id = "e1",
                    Kind = ResultKind.Document,
                    Title = "meeting-notes.txt",
                    Subtitle = @"C:\Docs\meeting-notes.txt",
                    MatchExplanation = "Exact prefix",
                    Score = 0.92,
                },
                new SearchResult
                {
                    Id = "e2",
                    Kind = ResultKind.Document,
                    Title = "meeting-agenda.txt",
                    Subtitle = @"C:\Docs\meeting-agenda.txt",
                    MatchExplanation = "Filename token",
                    Score = 0.55,
                },
            ],
            Done = false,
        });

        await WaitUntilAsync(() => vm.Results.Count == 2);

        // While waiting for semantic stage, IsSemanticPending must be true and exact results untouched
        Assert.True(vm.IsSemanticPending);
        Assert.False(vm.IsSearching);
        Assert.Equal(["e1", "e2"], vm.Results.Select(r => r.Result.Id).ToArray());

        // Emit semantic batch (Done = true) with higher score than e2 — must append without reordering!
        channel.Writer.TryWrite(new SearchBatch
        {
            Results =
            [
                new SearchResult
                {
                    Id = "s99",
                    Kind = ResultKind.Document,
                    Title = "banquet-hall.txt",
                    Subtitle = @"C:\Docs\banquet-hall.txt",
                    MatchExplanation = "semantic name match · similarity 0.88",
                    Score = 0.89,
                },
            ],
            Done = true,
        });
        channel.Writer.TryComplete();

        await searchTask;

        Assert.False(vm.IsSemanticPending);
        Assert.False(vm.IsSearching);
        Assert.Equal(["e1", "e2", "s99"], vm.Results.Select(r => r.Result.Id).ToArray());
        Assert.Equal("semantic name match · similarity 0.88", vm.Results[2].Explanation);
        Assert.Equal("3 results", vm.StatusText);
    }

    [Fact]
    public async Task MissingModelHint_ShowsWhenModelNotReady_AndHidesWhenReadyOrCodeOrDisabled()
    {
        var fake = new FakeEngineClient
        {
            CurrentConfig = new EngineConfig { SemanticEnabled = true, PreferredBackend = "auto" },
            CurrentModelStatus = new ModelStatus { State = ModelState.NotDownloaded },
        };

        await using var vm = new MainOverlayViewModel(fake, new AppSettings());
        await vm.InitializeAsync();

        // Empty query -> no hint
        Assert.Equal("", vm.SemanticHintText);

        // Normal query when model is missing -> exact hint shown
        vm.QueryText = "budget report";
        Assert.Equal(MainOverlayViewModel.MissingModelHintText, vm.SemanticHintText);
        Assert.Equal("Semantic search needs the AI model — Settings → AI model", vm.SemanticHintText);

        // code: query -> no semantic hint
        vm.QueryText = "code: retry backoff";
        Assert.Equal("", vm.SemanticHintText);

        // Back to normal query -> hint returns
        vm.QueryText = "budget report";
        Assert.Equal(MainOverlayViewModel.MissingModelHintText, vm.SemanticHintText);

        // Model becomes Ready -> hint disappears live
        fake.PushModelStatus(new ModelStatus { State = ModelState.Ready, Bytes = 100, TotalBytes = 100 });
        Assert.Equal("", vm.SemanticHintText);

        // Model deleted -> hint reappears
        fake.PushModelStatus(new ModelStatus { State = ModelState.NotDownloaded });
        Assert.Equal(MainOverlayViewModel.MissingModelHintText, vm.SemanticHintText);

        // User disables semantic search in Settings -> hint disappears and SearchQuery.IncludeSemantic is false
        fake.PushConfig(new EngineConfig { SemanticEnabled = false, PreferredBackend = "auto" });
        Assert.Equal("", vm.SemanticHintText);

        await vm.RunSearchAsync("budget report");
        Assert.False(fake.SearchCalls.Last().IncludeSemantic);
    }

    [Fact]
    public async Task SupersededSearch_ClearsIsSemanticPending_AndDiscardsStaleBatches()
    {
        var firstChannel = Channel.CreateUnbounded<SearchBatch>();
        var secondChannel = Channel.CreateUnbounded<SearchBatch>();
        var callIndex = 0;

        var fake = new FakeEngineClient
        {
            CurrentConfig = new EngineConfig { SemanticEnabled = true, PreferredBackend = "auto" },
            CurrentModelStatus = new ModelStatus { State = ModelState.Ready },
            SearchHandler = (_, ct) =>
            {
                var current = Interlocked.Increment(ref callIndex);
                return ReadBatchesAsync(current == 1 ? firstChannel.Reader : secondChannel.Reader, ct);
            },
        };

        await using var vm = new MainOverlayViewModel(fake, new AppSettings());
        await vm.InitializeAsync();

        var firstSearch = vm.RunSearchAsync("first");
        firstChannel.Writer.TryWrite(new SearchBatch
        {
            Results = [new SearchResult { Id = "e1", Title = "first.txt", Score = 0.9 }],
            Done = false,
        });

        await WaitUntilAsync(() => vm.IsSemanticPending);
        Assert.True(vm.IsSemanticPending);

        // Start a second search before the first semantic batch arrives
        var secondSearch = vm.RunSearchAsync("second");
        secondChannel.Writer.TryWrite(new SearchBatch
        {
            Results = [new SearchResult { Id = "e2", Title = "second.txt", Score = 0.95 }],
            Done = true,
        });
        secondChannel.Writer.TryComplete();

        await secondSearch;

        // Complete the stale first channel afterward — must be ignored
        firstChannel.Writer.TryWrite(new SearchBatch
        {
            Results = [new SearchResult { Id = "s1", Title = "stale-semantic.txt", Score = 0.85 }],
            Done = true,
        });
        firstChannel.Writer.TryComplete();
        await firstSearch;

        Assert.False(vm.IsSemanticPending);
        Assert.Single(vm.Results);
        Assert.Equal("e2", vm.Results[0].Result.Id);
    }

    private static async IAsyncEnumerable<SearchBatch> ReadBatchesAsync(
        ChannelReader<SearchBatch> reader,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        await foreach (var batch in reader.ReadAllAsync(cancellationToken))
        {
            yield return batch;
        }
    }

    private static async Task WaitUntilAsync(Func<bool> condition, int timeoutMs = 2000)
    {
        var start = Environment.TickCount64;
        while (!condition())
        {
            if (Environment.TickCount64 - start > timeoutMs)
            {
                throw new TimeoutException("Condition was not met within timeout.");
            }
            await Task.Delay(10);
        }
    }
}
